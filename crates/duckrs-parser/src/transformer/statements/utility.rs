// Ported from duckdb/src/parser/peg/transformer/{transform_generic_copy_option,transform_copy,
// transform_export,transform_attach,transform_analyze,transform_vacuum,transform_load}.cpp
// @ rust-port-baseline
//
//! COPY, EXPORT/IMPORT, ATTACH, VACUUM/ANALYZE and LOAD/INSTALL, with the generic option lists
//! they share.

use super::super::{Transformer, not_implemented, parser_error};
use crate::ast::*;
use crate::matcher::ParseNode;
use duckrs_common::{Error, ErrorKind, Result};

/// An option as written (`GenericCopyOption`): constant values, or an expression kept whole.
struct GenericOption {
    name: String,
    children: Vec<Literal>,
    expression: Option<Expr>,
}

impl GenericOption {
    fn value(name: &str, value: Literal) -> Self {
        Self {
            name: name.into(),
            children: vec![value],
            expression: None,
        }
    }
}

fn binder_error(message: String) -> Error {
    Error::new(ErrorKind::Binder, message)
}

/// `SetGenericCopyOptionExpression`.
fn set_option_expression(option: &mut GenericOption, expr: Expr) {
    match expr.kind {
        ExprKind::Constant(literal) => option.children.push(literal),
        ExprKind::ColumnRef(names) => option
            .children
            .push(Literal::string(names.last().cloned().unwrap_or_default())),
        ExprKind::Star(_) => option.children.push(Literal::string("*")),
        ExprKind::Cast { ref child, .. } => match &child.kind {
            ExprKind::Constant(l) if l.kind == LiteralKind::String && l.text == "t" => {
                option.children.push(Literal::boolean(true))
            }
            ExprKind::Constant(l) if l.kind == LiteralKind::String && l.text == "f" => {
                option.children.push(Literal::boolean(false))
            }
            _ => option.expression = Some(expr),
        },
        _ => option.expression = Some(expr),
    }
}

/// `ExtractFormat`: the file extension, ignoring a compression suffix.
fn extract_format(path: &str) -> String {
    let mut format = path.to_lowercase();
    if format.ends_with(".gz") {
        format.truncate(format.len() - 3);
    } else if format.ends_with(".zst") {
        format.truncate(format.len() - 4);
    }
    match format.rfind('.') {
        Some(dot) if dot + 1 < format.len() => format[dot + 1..].to_string(),
        _ => String::new(),
    }
}

impl Transformer<'_> {
    fn order_by_list(&mut self, node: &ParseNode) -> Result<Vec<OrderByNode>> {
        let mut orders = Vec::new();
        for o in self.subs(node, "OrderByExpression") {
            let expr = self.expr(self.req(o, "Expression")?)?;
            let (order, nulls) = self.order_modifiers(o)?;
            orders.push(OrderByNode { order, nulls, expr });
        }
        Ok(orders)
    }

    /// `BuildGenericCopyOption`.
    fn generic_option(&mut self, name: String, value: Option<&ParseNode>) -> Result<GenericOption> {
        let mut option = GenericOption {
            name: name.to_lowercase(),
            children: Vec::new(),
            expression: None,
        };
        let Some(value) = value else {
            if option.name == "partition_path" {
                return Err(parser_error("PARTITION_PATH expects an expression"));
            }
            return Ok(option);
        };
        let value = self.alt(value)?;
        if self.name(value) == "GenericCopyOptionExpression" {
            let expr = self.expr(self.req(value, "Expression")?)?;
            if option.name == "partition_path" {
                option.expression = Some(expr);
            } else {
                set_option_expression(&mut option, expr);
            }
            return Ok(option);
        }
        let list = self.req(
            self.req(value, "GenericCopyOptionParenthesizedExpressionList")?,
            "OrderByExpressionList",
        )?;
        let mut orders = self.order_by_list(list)?;
        let has_modifier = orders
            .iter()
            .any(|o| o.order != OrderType::Default || o.nulls != NullOrder::Default);
        if option.name == "partition_path" {
            if orders.len() != 1 || has_modifier {
                return Err(parser_error("PARTITION_PATH expects a single expression"));
            }
            option.expression = orders.pop().map(|o| o.expr);
        } else if option.name == "order_by" {
            let rendered = orders
                .iter()
                .map(|o| Expr::constant(Literal::string(o.to_string())))
                .collect();
            option.expression = Some(FunctionExpr::new("row", rendered).into());
        } else if has_modifier {
            return Err(parser_error(
                "ORDER BY modifiers are only supported in the ORDER BY option",
            ));
        } else if orders.len() == 1 {
            set_option_expression(&mut option, orders.pop().unwrap().expr);
        } else {
            let exprs = orders.into_iter().map(|o| o.expr).collect();
            option.expression = Some(FunctionExpr::new("row", exprs).into());
        }
        Ok(option)
    }

    /// `GenericCopyOptionList <- Parens(List(GenericCopyOption))` (and the COPY variant).
    fn generic_option_list(&mut self, node: &ParseNode, check_duplicates: bool) -> Result<Vec<GenericOption>> {
        let mut options: Vec<GenericOption> = Vec::new();
        for option in Self::parts(node) {
            let option = match self.name(option) {
                "GenericCopyOption" => option,
                "CopyGenericOption" => self.alt(option)?,
                _ => continue,
            };
            let value = self.sub(option, "GenericCopyOptionValue");
            let name = match self.name(option) {
                "OrderByCopyOption" => "order_by".to_string(),
                "PartitionedByCopyOption" => "partition_by".to_string(),
                _ => Self::text(self.req(option, "CopyOptionName")?),
            };
            let option = self.generic_option(name, value)?;
            if check_duplicates && options.iter().any(|o| o.name.eq_ignore_ascii_case(&option.name)) {
                return Err(parser_error(format!("Unexpected duplicate option {}", option.name)));
            }
            options.push(option);
        }
        Ok(options)
    }

    /// `SpecializedOptionList`: the PostgreSQL-style COPY options.
    fn specialized_options(&mut self, node: &ParseNode) -> Result<Vec<GenericOption>> {
        let mut options = Vec::new();
        for part in Self::parts(node) {
            let option = match self.name(part) {
                "SpecializedOption" => part,
                "SpecializedOptionTail" => self.req(part, "SpecializedOption")?,
                _ => continue,
            };
            let option = self.alt(option)?;
            let string =
                |s: &Self| -> Result<Literal> { Ok(Literal::string(Self::text(s.req(option, "StringLiteral")?))) };
            options.push(match self.name(option) {
                "SingleOption" => match self.name(self.alt(option)?) {
                    "BinaryOption" => GenericOption::value("format", Literal::string("binary")),
                    "FreezeOption" => GenericOption::value("freeze", Literal::null()),
                    "OidsOption" => GenericOption::value("oids", Literal::null()),
                    "CsvOption" => GenericOption::value("format", Literal::string("csv")),
                    _ => GenericOption::value("header", Literal::boolean(true)),
                },
                "NullAsOption" => GenericOption::value("null", string(self)?),
                "DelimiterAsOption" => GenericOption::value("delimiter", string(self)?),
                "EscapeAsOption" => GenericOption::value("escape", string(self)?),
                "EncodingOption" => GenericOption::value("encoding", string(self)?),
                "QuoteAsOption" => GenericOption::value("quote", string(self)?),
                "ForceQuoteOption" => {
                    let name = if self.sub(option, "ForceQuote").is_some() {
                        "force_quote"
                    } else {
                        "quote"
                    };
                    let columns = self.alt(self.req(option, "StarSymbolColumnList")?)?;
                    self.column_option(name, columns)?
                }
                "PartitionByOption" => {
                    let list = self.alt(self.req(option, "PartitionByColumnList")?)?;
                    match self.name(list) {
                        "StarPartitionByColumnList" => GenericOption {
                            name: "partition_by".into(),
                            children: Vec::new(),
                            expression: Some(Expr::star()),
                        },
                        "SinglePartitionByColumnList" => {
                            GenericOption::value("partition_by", Literal::string(Self::text(list)))
                        }
                        _ => self.column_option("partition_by", self.req(list, "ColumnList")?)?,
                    }
                }
                _ => {
                    let name = if self.sub(option, "ForceNotNull").is_some() {
                        "force_not_null"
                    } else {
                        "force_null"
                    };
                    self.column_option(name, self.req(option, "ColumnList")?)?
                }
            });
        }
        Ok(options)
    }

    /// An option naming columns (a `ColumnList`) or all of them (`*`).
    fn column_option(&self, name: &str, node: &ParseNode) -> Result<GenericOption> {
        if self.name(node) == "StarSymbol" {
            return Ok(GenericOption {
                name: name.into(),
                children: Vec::new(),
                expression: Some(Expr::star()),
            });
        }
        Ok(GenericOption {
            name: name.into(),
            children: self.texts(node, "ColId").into_iter().map(Literal::string).collect(),
            expression: None,
        })
    }

    /// `CopyOptions <- 'WITH'? CopyOptionList` or `ExportOptions`.
    fn copy_option_list(&mut self, node: &ParseNode) -> Result<Vec<GenericOption>> {
        let list = self.alt(node)?;
        match self.name(list) {
            "SpecializedOptionList" => self.specialized_options(list),
            "GenericCopyOptionList" => self.generic_option_list(list, true),
            _ => self.generic_option_list(list, false),
        }
    }

    /// `CopyFileName`: a constant path or an expression.
    fn copy_file_name(&mut self, info: &mut CopyInfo, node: &ParseNode) -> Result<()> {
        let name = self.alt(node)?;
        let path = match self.name(name) {
            "CopyFileNameExpression" => {
                let expr = self.expr(self.alt(name)?)?;
                match expr.kind {
                    ExprKind::Constant(literal) => literal.text,
                    _ => {
                        info.file_path_expression = Some(Box::new(expr));
                        return Ok(());
                    }
                }
            }
            "CopyFileNameIdentifierColId" => {
                let id = self.req(name, "IdentifierColId")?;
                let mut parts = vec![Self::text(self.req(id, "Identifier")?)];
                for suffix in self.subs(id, "CopyFileNameSuffix") {
                    parts.push(Self::text(self.req(suffix, "ColId")?));
                }
                parts.join(".")
            }
            "CopyFileNameIdentifier" => {
                let id = Self::text(name);
                match id.to_lowercase().as_str() {
                    "stdin" => "/dev/stdin".into(),
                    "stdout" => "/dev/stdout".into(),
                    _ => id,
                }
            }
            _ => Self::text(name),
        };
        info.file_path = path;
        Ok(())
    }

    /// `SetCopyOptions`.
    fn set_copy_options(&self, info: &mut CopyInfo, options: Vec<GenericOption>) -> Result<()> {
        let mut seen: Vec<String> = Vec::new();
        for mut option in options {
            if option.name.eq_ignore_ascii_case("partitioned_by") {
                option.name = "partition_by".into();
            }
            if seen.iter().any(|n| n.eq_ignore_ascii_case(&option.name)) {
                return Err(parser_error(format!("Unexpected duplicate option {}", option.name)));
            }
            seen.push(option.name.clone());
            let lower = option.name.to_lowercase();
            match lower.as_str() {
                "partition_by" | "force_quote" | "force_not_null" | "force_null" => {
                    let expr = match option.expression {
                        Some(e) => e,
                        None => {
                            if option.children.is_empty() {
                                return Err(binder_error(format!(
                                    "{} expects a column list or * as parameter",
                                    option.name
                                )));
                            }
                            let columns = option
                                .children
                                .into_iter()
                                .map(|c| Expr::column(vec![c.text]))
                                .collect();
                            FunctionExpr::new("row", columns).into()
                        }
                    };
                    info.parsed_options.push((option.name, Some(expr)));
                }
                "header" | "escape" | "null" | "nullstr" => {
                    let expr = match (option.expression, option.children.into_iter().next()) {
                        (Some(e), _) => Some(e),
                        (None, Some(value)) => Some(Expr::constant(value)),
                        (None, None) => None,
                    };
                    info.parsed_options.push((option.name, expr));
                }
                _ => match option.expression {
                    Some(e) => info.parsed_options.push((option.name, Some(e))),
                    None => info.options.push((option.name, option.children)),
                },
            }
        }
        if let Some(i) = info.options.iter().position(|(n, _)| n.eq_ignore_ascii_case("format")) {
            let (_, values) = info.options.remove(i);
            let Some(format) = values.into_iter().next() else {
                return Err(parser_error(
                    "Unsupported parameter type for FORMAT: expected e.g. FORMAT 'csv', 'parquet'",
                ));
            };
            info.format = format.text;
            info.format_given = true;
        }
        Ok(())
    }

    pub(in crate::transformer) fn copy(&mut self, node: &ParseNode) -> Result<Statement> {
        let variation = self.alt(self.req(node, "CopyVariations")?)?;
        let mut info = CopyInfo::default();
        match self.name(variation) {
            "CopySelect" => {
                let query = self.select_internal(self.req(variation, "SelectStatementInternal")?)?;
                info.select = Some(Box::new(query.node));
                self.copy_file_name(&mut info, self.req(variation, "CopyFileName")?)?;
                if let Some(options) = self.sub(variation, "CopyOptions") {
                    let options = self.copy_option_list(self.req(options, "CopyOptionList")?)?;
                    self.set_copy_options(&mut info, options)?;
                }
            }
            "CopyTable" => {
                info.table = self.table_name(self.req(variation, "BaseTableName")?)?;
                if let Some(list) = self.sub(variation, "InsertColumnList") {
                    info.select_list = self.texts(self.req(list, "ColumnList")?, "ColId");
                }
                info.is_from = self.name(self.alt(self.req(variation, "FromOrTo")?)?) == "CopyFrom";
                self.copy_file_name(&mut info, self.req(variation, "CopyFileName")?)?;
                info.format = extract_format(&info.file_path);
                let mut options = Vec::new();
                if let Some(legacy) = self.sub(variation, "CopyLegacyOption") {
                    let delimiter = Self::text(self.req(legacy, "StringLiteral")?);
                    options.push(GenericOption::value("delimiter", Literal::string(delimiter)));
                }
                if let Some(copy_options) = self.sub(variation, "CopyOptions") {
                    options.extend(self.copy_option_list(self.req(copy_options, "CopyOptionList")?)?);
                }
                if !options.is_empty() {
                    self.set_copy_options(&mut info, options)?;
                }
            }
            _ => {
                let database = self.alt(variation)?;
                let names = self.texts(database, "ColId");
                if self.name(database) == "CopyFromDatabaseWithFlag" {
                    let flag = self.req(self.req(database, "CopyDatabaseFlag")?, "SchemaOrData")?;
                    return Ok(Statement::CopyDatabase {
                        from: names[0].clone(),
                        to: names[1].clone(),
                        schema_only: self.name(self.alt(flag)?) == "CopySchema",
                    });
                }
                return Ok(Statement::Pragma(PragmaInfo {
                    name: "copy_database".into(),
                    parameters: names.into_iter().map(|n| Expr::constant(Literal::string(n))).collect(),
                    named_parameters: Vec::new(),
                }));
            }
        }
        Ok(Statement::Copy(Box::new(info)))
    }

    /// `CREATE SECRET`: PROVIDER, TYPE and SCOPE are kept apart from the other options.
    pub(in crate::transformer) fn create_secret(&mut self, node: &ParseNode, persistent: bool) -> Result<CreateKind> {
        let storage = match self.sub(node, "SecretStorageSpecifier") {
            Some(s) => Self::text(self.req(s, "Identifier")?).to_lowercase(),
            None => String::new(),
        };
        let (mut provider, mut secret_type, mut scope) = (None, None, None);
        let mut options: Vec<(String, Expr)> = Vec::new();
        for option in self.generic_option_list(self.req(node, "GenericCopyOptionList")?, true)? {
            let value = match (option.children.into_iter().next(), option.expression) {
                (Some(value), _) => Expr::constant(value),
                (None, Some(e)) => e,
                (None, None) => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "No expression in GenericCopyOption",
                    ));
                }
            };
            let name = option.name.to_lowercase();
            match name.as_str() {
                "scope" => scope = Some(Box::new(value)),
                "type" => secret_type = Some(Box::new(value)),
                "provider" => provider = Some(Box::new(value)),
                _ => {
                    if options.iter().any(|(n, _)| *n == name) {
                        return Err(binder_error(format!(
                            "Duplicate query param found while parsing create secret: '{name}'"
                        )));
                    }
                    options.push((name, value));
                }
            }
        }
        Ok(CreateKind::Secret {
            persistent,
            storage,
            provider,
            secret_type,
            scope,
            options,
        })
    }

    pub(in crate::transformer) fn export(&mut self, node: &ParseNode) -> Result<Statement> {
        let mut info = CopyInfo {
            file_path: Self::text(self.req(node, "StringLiteral")?),
            format: "csv".into(),
            ..CopyInfo::default()
        };
        if let Some(options) = self.sub(node, "ExportOptions") {
            for option in self.copy_option_list(options)? {
                if option.name == "format" {
                    if option.expression.is_some() {
                        return Err(parser_error(
                            "Unsupported parameter type for FORMAT: expected e.g. FORMAT 'csv', 'parquet'",
                        ));
                    }
                    let Some(format) = option.children.into_iter().next() else {
                        return Err(parser_error(
                            "FORMAT requires a parameter, e.g. FORMAT 'csv' or FORMAT 'parquet'",
                        ));
                    };
                    info.format = format.text;
                    info.format_given = true;
                } else if let Some(e) = option.expression {
                    info.parsed_options.push((option.name, Some(e)));
                } else {
                    info.options.push((option.name, option.children));
                }
            }
        }
        let database = match self.sub(node, "ExportSource") {
            Some(source) => Self::text(self.req(source, "CatalogName")?),
            None => String::new(),
        };
        Ok(Statement::Export {
            database,
            info: Box::new(info),
        })
    }

    pub(in crate::transformer) fn attach(&mut self, node: &ParseNode) -> Result<Statement> {
        let or_replace = self.sub(node, "OrReplace").is_some();
        let if_not_exists = self.sub(node, "IfNotExists").is_some();
        if or_replace && if_not_exists {
            return Err(parser_error(
                "Cannot specify both OR REPLACE and IF NOT EXISTS at the same time",
            ));
        }
        let mut info = AttachInfo {
            on_conflict: if or_replace {
                OnCreateConflict::Replace
            } else if if_not_exists {
                OnCreateConflict::Ignore
            } else {
                OnCreateConflict::Error
            },
            path: Box::new(self.expr(self.req(self.req(node, "DatabasePath")?, "Expression")?)?),
            name: match self.sub(node, "AttachAlias") {
                Some(alias) => Self::text(self.req(alias, "ColId")?),
                None => String::new(),
            },
            parsed_options: Vec::new(),
            options: Vec::new(),
        };
        if let Some(options) = self.sub(node, "AttachOptions") {
            for option in self.generic_option_list(self.req(options, "GenericCopyOptionList")?, true)? {
                if let Some(e) = option.expression {
                    info.parsed_options.push((option.name, e));
                    continue;
                }
                let value = match option.children.len() {
                    0 => Literal::boolean(true),
                    1 => option.children.into_iter().next().unwrap(),
                    _ => {
                        return Err(parser_error(format!(
                            "Option {} can only have one argument",
                            option.name
                        )));
                    }
                };
                if value.kind == LiteralKind::Null {
                    return Err(binder_error(format!(
                        "NULL is not supported as a valid option for ATTACH option {}",
                        option.name
                    )));
                }
                info.options.retain(|(n, _)| !n.eq_ignore_ascii_case(&option.name));
                info.options.push((option.name, value));
            }
        }
        Ok(Statement::Attach(Box::new(info)))
    }

    fn analyze_target(&self, node: &ParseNode) -> Result<(Option<Box<TableRef>>, Vec<String>)> {
        let Some(target) = self.sub(node, "AnalyzeTarget") else {
            return Ok((None, Vec::new()));
        };
        let table = self.base_table_name(self.req(target, "BaseTableName")?)?;
        let columns = match self.sub(target, "NameList") {
            Some(list) => self.texts(list, "ColIdOrString"),
            None => Vec::new(),
        };
        Ok((Some(Box::new(table)), columns))
    }

    pub(in crate::transformer) fn analyze(&self, node: &ParseNode) -> Result<Statement> {
        if self.sub(node, "AnalyzeVerbose").is_some() {
            return Err(not_implemented("ANALYZE VERBOSE is not implemented yet"));
        }
        let (table, columns) = self.analyze_target(node)?;
        Ok(Statement::Vacuum {
            analyze: true,
            table,
            columns,
        })
    }

    pub(in crate::transformer) fn vacuum(&self, node: &ParseNode) -> Result<Statement> {
        let mut analyze = false;
        if let Some(options) = self.sub(node, "VacuumOptions") {
            let options = self.alt(options)?;
            if self.name(options) == "VacuumLegacyOptions" {
                for (rule, what) in [("OptFull", "FULL"), ("OptFreeze", "FREEZE"), ("OptVerbose", "VERBOSE")] {
                    if self.sub(options, rule).is_some() {
                        return Err(not_implemented(format!("{what} is not yet implemented")));
                    }
                }
                analyze = self.sub(options, "OptAnalyze").is_some();
            } else {
                for option in self.subs(options, "VacuumOption") {
                    let option = self.alt(option)?;
                    let name = match self.name(option) {
                        "OptAnalyze" => "analyze".to_string(),
                        _ => Self::text(option),
                    };
                    match name.to_lowercase().as_str() {
                        "disable_page_skipping" => {
                            return Err(not_implemented("Disable Page Skipping vacuum option"));
                        }
                        "freeze" => return Err(not_implemented("FREEZE is not yet implemented")),
                        "full" => return Err(not_implemented("FULL is not yet implemented")),
                        "verbose" => return Err(not_implemented("VERBOSE is not yet implemented")),
                        "analyze" => analyze = true,
                        _ => return Err(parser_error(format!("unrecognized VACUUM option \"{name}\""))),
                    }
                }
            }
        }
        let (table, columns) = self.analyze_target(node)?;
        Ok(Statement::Vacuum {
            analyze,
            table,
            columns,
        })
    }

    /// `FromSource`: an extension repository by alias or URL.
    fn repository_source(&self, node: &ParseNode, info: &mut LoadInfo) -> Result<()> {
        if let Some(source) = self.sub(node, "FromSource") {
            let source = self.alt(source)?;
            info.repo_is_alias = self.name(source) == "FromSourceIdentifier";
            info.repository = Self::text(self.alt(source)?);
        }
        Ok(())
    }

    fn load_info(load_type: LoadType) -> LoadInfo {
        LoadInfo {
            load_type,
            load_after_install: false,
            filename: String::new(),
            repository: String::new(),
            repo_is_alias: false,
            alias: String::new(),
            version: String::new(),
            repository_url: String::new(),
            public_keys: Vec::new(),
            on_conflict: OnCreateConflict::Error,
            missing_ok: false,
        }
    }

    pub(in crate::transformer) fn load(&self, node: &ParseNode) -> Result<Statement> {
        let mut info = Self::load_info(LoadType::Load);
        info.filename = Self::text(self.req(node, "ColIdOrString")?);
        self.repository_source(node, &mut info)?;
        if let Some(alias) = self.sub(node, "ExtensionAlias") {
            info.alias = Self::text(self.req(alias, "Identifier")?);
        }
        Ok(Statement::Load(Box::new(info)))
    }

    pub(in crate::transformer) fn install(&self, node: &ParseNode) -> Result<Statement> {
        let load_type = if self.has(node, "FORCE") {
            LoadType::ForceInstall
        } else {
            LoadType::Install
        };
        let mut info = Self::load_info(load_type);
        info.load_after_install = self.sub(node, "InstallAndLoad").is_some();
        info.filename = Self::text(self.req(node, "IdentifierOrStringLiteral")?);
        self.repository_source(node, &mut info)?;
        if let Some(version) = self.sub(node, "VersionNumber") {
            info.version = Self::text(self.req(version, "IdentifierOrStringLiteral")?);
        }
        Ok(Statement::Load(Box::new(info)))
    }

    pub(in crate::transformer) fn extension_repository(&self, node: &ParseNode) -> Result<Statement> {
        let statement = self.alt(node)?;
        let repository = Self::text(self.req(statement, "ColIdOrString")?);
        if self.name(statement) == "DropExtensionRepositoryStmt" {
            let mut info = Self::load_info(LoadType::DropRepository);
            info.repository = repository;
            info.missing_ok = self.sub(statement, "IfExists").is_some();
            return Ok(Statement::Load(Box::new(info)));
        }
        let mut info = Self::load_info(LoadType::CreateRepository);
        info.repository = repository;
        info.repository_url = Self::text(self.req(self.req(statement, "RepositoryPrefix")?, "StringLiteral")?);
        if let Some(keys) = self.sub(statement, "RepositoryPublicKey") {
            info.public_keys = self.texts(keys, "StringLiteral");
        }
        let or_replace = self.sub(statement, "OrReplace").is_some();
        let if_not_exists = self.sub(statement, "IfNotExists").is_some();
        if or_replace && if_not_exists {
            return Err(parser_error("Cannot combine OR REPLACE with IF NOT EXISTS"));
        }
        info.on_conflict = if or_replace {
            OnCreateConflict::Replace
        } else if if_not_exists {
            OnCreateConflict::Ignore
        } else {
            OnCreateConflict::Error
        };
        Ok(Statement::Load(Box::new(info)))
    }
}
