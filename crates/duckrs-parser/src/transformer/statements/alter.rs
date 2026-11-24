// Ported from duckdb/src/parser/peg/transformer/{transform_alter,transform_comment,transform_describe}.cpp
// @ rust-port-baseline
//
//! ALTER, COMMENT ON and SHOW / DESCRIBE / SUMMARIZE.

use super::super::{Transformer, not_implemented, parser_error};
use super::{COMPRESSIONS, apply_collation};
use crate::ast::*;
use crate::matcher::ParseNode;
use duckrs_common::Result;

/// `IsSimpleDefaultValue`: a constant, possibly cast (but not TRY_CAST).
fn is_simple_default(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Cast {
            child, try_cast: false, ..
        } => matches!(child.kind, ExprKind::Constant(_)),
        ExprKind::Constant(_) => true,
        _ => false,
    }
}

/// An ADD COLUMN entry with the constraints that need follow-up statements.
struct AddColumnEntry {
    path: Vec<String>,
    column: ColumnDefinition,
    not_null: bool,
    unique: bool,
}

impl Transformer<'_> {
    pub(in crate::transformer) fn alter(&mut self, node: &ParseNode) -> Result<Statement> {
        let options = self.alt(self.req(node, "AlterOptions")?)?;
        let if_exists = self.sub(options, "IfExists").is_some();
        let info = match self.name(options) {
            "AlterTableStmt" => {
                let entries = self.subs(options, "AlterTableOptions");
                if entries.len() > 1 {
                    return Err(parser_error("Only one ALTER command per statement is supported"));
                }
                let name = self.table_name(self.req(options, "BaseTableName")?)?;
                let kind = self.alter_table_option(self.alt(entries[0])?)?;
                return self.finish_alter_table(AlterInfo { name, if_exists, kind });
            }
            "AlterViewStmt" => AlterInfo {
                name: self.table_name(self.req(options, "BaseTableName")?)?,
                if_exists,
                kind: AlterKind::RenameView(Self::text(
                    self.req(self.req(options, "RenameAlter")?, "ColIdOrString")?,
                )),
            },
            "AlterSequenceStmt" => {
                let name = QualifiedName::from_parts(self.rule_texts(self.req(options, "QualifiedSequenceName")?));
                let sequence_options = self.alt(self.req(options, "AlterSequenceOptions")?)?;
                if self.name(sequence_options) == "RenameAlterSequenceOptions" {
                    return Err(not_implemented("Renaming sequences is not yet supported"));
                }
                let mut owner = None;
                for option in self.subs(sequence_options, "SequenceOption") {
                    let option = self.alt(option)?;
                    if self.name(option) != "SeqOwnedBy" {
                        continue;
                    }
                    if owner.is_some() {
                        return Err(parser_error("Owned by value should be passed at most once"));
                    }
                    owner = Some(self.qualified_name(self.req(option, "QualifiedName")?)?);
                }
                let Some(owner) = owner else {
                    return Err(not_implemented("ALTER SEQUENCE option not yet supported"));
                };
                AlterInfo {
                    name,
                    if_exists,
                    kind: AlterKind::ChangeOwnership {
                        owner_schema: owner.qualification.last().cloned().unwrap_or_else(|| "main".into()),
                        owner_name: owner.name,
                    },
                }
            }
            "AlterDatabaseStmt" => {
                let names = self.texts(options, "Identifier");
                AlterInfo {
                    name: QualifiedName::new(names[0].clone()),
                    if_exists,
                    kind: AlterKind::RenameDatabase(names[1].clone()),
                }
            }
            _ => {
                let name = self.qualified_name(self.req(options, "QualifiedName")?)?;
                let schema_options = self.alt(self.req(options, "AlterSchemaOptions")?)?;
                let kind = match self.name(schema_options) {
                    "SetOptions" => {
                        AlterKind::SetSchemaOptions(self.rel_options(self.req(schema_options, "RelOptionList")?)?)
                    }
                    "ResetOptions" => AlterKind::ResetSchemaOptions(self.reset_options(schema_options)?),
                    _ => return Err(not_implemented("Altering schemas is not yet supported")),
                };
                AlterInfo { name, if_exists, kind }
            }
        };
        Ok(Statement::Alter(Box::new(info)))
    }

    /// RESET (options): the options may not carry values.
    fn reset_options(&mut self, node: &ParseNode) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for (name, value) in self.rel_options(self.req(node, "RelOptionList")?)? {
            if !matches!(&value.kind, ExprKind::Constant(l) if l.kind == LiteralKind::Null) {
                return Err(parser_error(format!(
                    "Reset option \"{name}\" cannot set any value. Did you mean to use SET?"
                )));
            }
            names.push(name);
        }
        Ok(names)
    }

    fn alter_table_option(&mut self, node: &ParseNode) -> Result<AlterKind> {
        let path = |s: &Self, n: &ParseNode| -> Result<Vec<String>> { Ok(s.rule_texts(s.req(n, "NestedColumnName")?)) };
        Ok(match self.name(node) {
            "AddColumn" => {
                let entry = self.add_column_entry(self.req(node, "AddColumnEntry")?)?;
                let if_not_exists = self.sub(node, "IfNotExists").is_some();
                if entry.path.len() == 1 {
                    AlterKind::AddColumn {
                        column: entry.column,
                        if_not_exists,
                        not_null: entry.not_null,
                        unique: entry.unique,
                    }
                } else {
                    if entry.not_null {
                        return Err(not_implemented(
                            "Adding NOT NULL constraints to nested fields is not supported",
                        ));
                    }
                    if entry.unique {
                        return Err(not_implemented(
                            "Adding UNIQUE constraints to nested fields is not supported",
                        ));
                    }
                    if entry.column.compression.is_some() {
                        return Err(not_implemented("Adding compression to nested fields is not supported"));
                    }
                    let mut parent = entry.path;
                    parent.pop();
                    AlterKind::AddField {
                        parent,
                        column: entry.column,
                        if_not_exists,
                    }
                }
            }
            "DropConstraint" => return Err(not_implemented("No support for that ALTER TABLE option yet!")),
            "DropColumn" => AlterKind::RemoveColumn {
                path: path(self, node)?,
                if_exists: self.sub(node, "IfExists").is_some(),
                cascade: match self.sub(node, "DropBehavior") {
                    Some(b) => self.cascade(b)?,
                    None => false,
                },
            },
            "AlterColumn" => {
                let path = path(self, node)?;
                let entry = self.alt(self.req(node, "AlterColumnEntry")?)?;
                match self.name(entry) {
                    "AddOrDropDefault" => {
                        let default = self.alt(entry)?;
                        let expression = match self.name(default) {
                            "AddDefault" => Some(Box::new(self.expr(self.req(default, "Expression")?)?)),
                            _ => None,
                        };
                        AlterKind::SetDefault { path, expression }
                    }
                    "ChangeNullability" => {
                        if self.name(self.alt(self.req(entry, "DropOrSet")?)?) == "DropNullability" {
                            AlterKind::DropNotNull(path)
                        } else {
                            AlterKind::SetNotNull(path)
                        }
                    }
                    _ => {
                        let mut target = match self.sub(entry, "Type") {
                            Some(t) => Some(self.type_(t)?),
                            None => None,
                        };
                        let using = match self.sub(entry, "UsingExpression") {
                            Some(u) => Some(Box::new(self.expr(self.req(u, "Expression")?)?)),
                            None => None,
                        };
                        if target.is_none() && using.is_none() {
                            return Err(parser_error(
                                "Omitting the type is only possible in combination with USING",
                            ));
                        }
                        if let Some(collation) = self.sub(entry, "ColumnCollation") {
                            target = Some(apply_collation(target, self.collation_name(collation)?)?);
                        }
                        AlterKind::ChangeColumnType { path, target, using }
                    }
                }
            }
            "AddConstraint" => {
                AlterKind::AddConstraint(self.top_level_constraint(self.req(node, "TopLevelConstraint")?)?)
            }
            "RenameColumn" => AlterKind::RenameColumn {
                path: path(self, node)?,
                new_name: Self::text(self.req(node, "ColIdOrString")?),
            },
            "RenameAlter" => AlterKind::RenameTable(Self::text(self.req(node, "ColIdOrString")?)),
            "SetPartitionedBy" => AlterKind::SetPartitionedBy(self.exprs(node, "Expression")?),
            "ResetPartitionedBy" => AlterKind::SetPartitionedBy(Vec::new()),
            "SetSortedBy" => AlterKind::SetSortedBy(self.order_by_clause(node)?),
            "ResetSortedBy" => AlterKind::SetSortedBy(Vec::new()),
            "SetOptions" => AlterKind::SetTableOptions(self.rel_options(self.req(node, "RelOptionList")?)?),
            _ => AlterKind::ResetTableOptions(self.reset_options(node)?),
        })
    }

    /// `COLLATE a.b` as the dotted collation name.
    fn collation_name(&self, node: &ParseNode) -> Result<String> {
        let dotted = self.req(node, "DottedIdentifier")?;
        let mut parts = vec![Self::text(self.req(dotted, "Identifier")?)];
        for label in self.subs(dotted, "DotColLabel") {
            parts.push(Self::text(self.req(label, "ColLabel")?));
        }
        Ok(parts.join("."))
    }

    fn add_column_entry(&mut self, node: &ParseNode) -> Result<AddColumnEntry> {
        let dotted = self.req(node, "DottedIdentifier")?;
        let mut path = vec![Self::text(self.req(dotted, "Identifier")?)];
        for label in self.subs(dotted, "DotColLabel") {
            path.push(Self::text(self.req(label, "ColLabel")?));
        }
        let mut column_type = match self.sub(node, "Type") {
            Some(t) => Some(self.type_(t)?),
            None => None,
        };
        let generated = self.sub(node, "GeneratedColumn").is_some();
        if column_type.is_none() && !generated {
            return Err(parser_error(
                "Column definition requires a type or generated expression",
            ));
        }
        if generated {
            return Err(parser_error(
                "Adding generated columns after table creation is not supported yet",
            ));
        }
        let mut entry = AddColumnEntry {
            path,
            column: ColumnDefinition {
                name: String::new(),
                column_type: None,
                generated: None,
                default_value: None,
                compression: None,
            },
            not_null: false,
            unique: false,
        };
        for constraint in self.subs(node, "ColumnConstraint") {
            let constraint = self.alt(constraint)?;
            if self.name(constraint) == "ColumnCollation" {
                column_type = Some(apply_collation(column_type, self.collation_name(constraint)?)?);
                continue;
            }
            let element = self.alt(self.req(constraint, "ColumnConstraintElement")?)?;
            match self.name(element) {
                "DefaultValue" => {
                    if entry.column.default_value.is_some() {
                        return Err(parser_error("Cannot define a default value twice"));
                    }
                    let expr = self.expr(self.req(element, "ColumnDefaultExpr")?)?;
                    entry.column.default_value = Some(Box::new(expr));
                }
                "NotNullConstraint" => {
                    if self.name(self.alt(element)?) == "NotNullColumnConstraint" {
                        entry.not_null = true;
                    }
                }
                "PrimaryKeyConstraint" => {
                    return Err(parser_error(
                        "Adding columns with PRIMARY KEY constraints is not supported yet",
                    ));
                }
                "UniqueConstraint" => {
                    if self.check_mode(element)? != ConstraintCheckMode::Default {
                        return Err(parser_error(
                            "Adding columns with UNIQUE constraints with a check mode is not supported yet",
                        ));
                    }
                    entry.unique = true;
                }
                "CheckConstraint" => {
                    return Err(parser_error(
                        "Adding columns with CHECK constraints is not supported yet",
                    ));
                }
                "ForeignKeyConstraint" => {
                    return Err(parser_error(
                        "Adding columns with FOREIGN KEY constraints is not supported yet",
                    ));
                }
                _ => {
                    let requested = Self::text(self.req(element, "ColIdOrString")?).to_lowercase();
                    if requested == "auto" {
                        return Err(parser_error("Unrecognized option for column compression"));
                    }
                    let name = COMPRESSIONS
                        .iter()
                        .find(|(n, _)| *n == requested)
                        .map(|(_, display)| *display)
                        .ok_or_else(|| super::unrecognized_compression(&requested))?;
                    entry.column.compression = Some(name);
                }
            }
        }
        entry.column.name = entry.path.last().cloned().unwrap_or_default();
        entry.column.column_type = column_type;
        Ok(entry)
    }

    /// ADD COLUMN with NOT NULL, UNIQUE or a non-constant default becomes several statements: the
    /// column is added plainly (with a NULL default that an UPDATE then fills, so WAL replay is
    /// deterministic) and the constraints are added afterwards.
    fn finish_alter_table(&mut self, info: AlterInfo) -> Result<Statement> {
        let AlterKind::AddColumn {
            column,
            if_not_exists,
            not_null,
            unique,
        } = &info.kind
        else {
            return Ok(Statement::Alter(Box::new(info)));
        };
        let materialize = column.default_value.as_deref().is_some_and(|d| !is_simple_default(d));
        if !not_null && !unique && !materialize {
            return Ok(Statement::Alter(Box::new(info)));
        }
        if *if_not_exists {
            if *not_null {
                return Err(not_implemented(
                    "Adding a NOT NULL column with IF NOT EXISTS is not supported",
                ));
            }
            if *unique {
                return Err(not_implemented(
                    "Adding a UNIQUE column with IF NOT EXISTS is not supported",
                ));
            }
            return Ok(Statement::Alter(Box::new(info)));
        }
        let (not_null, unique) = (*not_null, *unique);
        let name = info.name.clone();
        let if_exists = info.if_exists;
        let column_name = column.name.clone();
        let alter = |kind: AlterKind| {
            Statement::Alter(Box::new(AlterInfo {
                name: name.clone(),
                if_exists,
                kind,
            }))
        };
        let mut column = column.clone();
        let mut statements = Vec::new();
        if materialize {
            let default = column.default_value.take().expect("a default to materialize");
            column.default_value = Some(Box::new(Expr::constant(Literal::null())));
            statements.push(alter(AlterKind::AddColumn {
                column,
                if_not_exists: false,
                not_null: false,
                unique: false,
            }));
            statements.push(Statement::Update(Box::new(UpdateStatement {
                cte_map: CteMap::default(),
                table: TableRefKind::BaseTable {
                    name: name.clone(),
                    at_clause: None,
                }
                .into(),
                set_info: UpdateSetInfo {
                    columns: vec![column_name.clone()],
                    expressions: vec![(*default).clone()],
                    condition: None,
                },
                from_table: None,
                returning: Vec::new(),
            })));
            statements.push(alter(AlterKind::SetDefault {
                path: vec![column_name.clone()],
                expression: Some(default),
            }));
        } else {
            statements.push(alter(AlterKind::AddColumn {
                column,
                if_not_exists: false,
                not_null: false,
                unique: false,
            }));
        }
        if not_null {
            statements.push(alter(AlterKind::SetNotNull(vec![column_name.clone()])));
        }
        if unique {
            statements.push(alter(AlterKind::AddConstraint(Constraint::Unique {
                index: None,
                columns: vec![column_name],
                primary_key: false,
                check_mode: ConstraintCheckMode::Default,
            })));
        }
        Ok(Statement::Multi(statements))
    }

    pub(in crate::transformer) fn comment(&mut self, node: &ParseNode) -> Result<Statement> {
        let entry_type = match self.name(self.alt(self.req(node, "CommentOnType")?)?) {
            "CommentTable" => "TABLE",
            "CommentSequence" => "SEQUENCE",
            "CommentFunction" | "CommentMacro" => "MACRO",
            "CommentMacroTable" => "MACRO TABLE",
            "CommentView" => "VIEW",
            "CommentIndex" => "INDEX",
            "CommentType" => "TYPE",
            "CommentDatabase" => return Err(not_implemented("Adding comments to databases is not implemented")),
            "CommentSchema" => return Err(not_implemented("Adding comments to schemas is not implemented")),
            _ => "COLUMN",
        };
        let target = self.alt(self.req(node, "CommentTarget")?)?;
        let mut path = if self.name(target) == "DottedIdentifier" {
            let mut parts = vec![Self::text(self.req(target, "Identifier")?)];
            for label in self.subs(target, "DotColLabel") {
                parts.push(Self::text(self.req(label, "ColLabel")?));
            }
            parts
        } else {
            vec![Self::text(target)]
        };
        let value = match self.name(self.alt(self.req(node, "CommentValue")?)?) {
            "NullLiteral" => Literal::null(),
            _ => Literal::string(Self::text(self.req(node, "CommentValue")?)),
        };
        let kind = if entry_type == "COLUMN" {
            let column = path.pop().unwrap_or_default();
            if path.is_empty() {
                return Err(parser_error(format!(
                    "Invalid column reference: {}",
                    sql_identifier(&column)
                )));
            }
            AlterKind::SetColumnComment { column, value }
        } else {
            AlterKind::SetComment { entry_type, value }
        };
        Ok(Statement::Alter(Box::new(AlterInfo {
            name: QualifiedName::from_parts(path),
            if_exists: false,
            kind,
        })))
    }

    /// `DescribeStatement`: SHOW / DESCRIBE / SUMMARIZE as `SELECT * FROM <show ref>`.
    pub(in crate::transformer) fn describe(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let form = self.alt(node)?;
        let show_type = |s: &Self, n: &ParseNode| -> Result<ShowType> {
            let rule = s.alt(n)?;
            Ok(match s.name(rule) {
                "ShowRule" => ShowType::Show,
                "Summarize" => ShowType::Summary,
                "DescribeRule" => ShowType::Describe,
                _ => s.show_type_of(rule)?,
            })
        };
        let mut show = ShowRef {
            show_type: ShowType::Describe,
            name: None,
            catalog: String::new(),
            schema: String::new(),
            query: None,
        };
        let all_tables = |show: &mut ShowRef| {
            show.name = Some(QualifiedName::new("__show_tables_expanded"));
            show.show_type = ShowType::ShowSpecial;
        };
        match self.name(form) {
            "ShowExternalResources" => return self.show_external_resources(form),
            "ShowTables" => {
                let name = self.qualified_name(self.req(form, "QualifiedName")?)?;
                if name.qualification.len() > 1 {
                    return Err(parser_error(
                        "Expected \"SHOW TABLES FROM database\", \"SHOW TABLES FROM schema\", or \"SHOW TABLES FROM \
                         database.schema\"",
                    ));
                }
                show.show_type = ShowType::ShowFrom;
                show.catalog = name.qualification.first().cloned().unwrap_or_default();
                show.schema = name.name;
            }
            "ShowAllTables" => all_tables(&mut show),
            "ShowDeprecatedSelect" | "DescribeSelect" => {
                show.show_type = if self.name(form) == "ShowDeprecatedSelect" {
                    ShowType::Show
                } else {
                    show_type(self, self.req(form, "DescribeOrSummarize")?)?
                };
                let query = self.select_internal(self.req(form, "SelectStatementInternal")?)?;
                show.query = Some(Box::new(query.node));
            }
            "ShowByName" => {
                show.show_type = ShowType::Show;
                match self.sub(form, "ShowTarget") {
                    None => all_tables(&mut show),
                    Some(target) => {
                        let target = self.alt(target)?;
                        match self.name(target) {
                            "DescribeStringLiteral" => self.describe_literal(target, &mut show)?,
                            "ShowDeprecatedQualifiedTableName" => {
                                let table = self.base_table_from_qualified(self.req(target, "QualifiedTableName")?)?;
                                if !special_show_form(&mut show, &table) {
                                    show.name = Some(table.clone());
                                    show.query = Some(Box::new(describe_query(table)));
                                }
                            }
                            _ => {
                                let table = QualifiedName::new(Self::text(target));
                                if !special_show_form(&mut show, &table) {
                                    show.name = Some(table);
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                show.show_type = show_type(self, self.req(form, "DescribeOrSummarize")?)?;
                match self.sub(form, "DescribeTarget") {
                    None => {
                        if show.show_type == ShowType::Summary {
                            return Err(parser_error("Expected table name with SUMMARIZE"));
                        }
                        all_tables(&mut show);
                    }
                    Some(target) => {
                        let target = self.alt(target)?;
                        if self.name(target) == "DescribeStringLiteral" {
                            self.describe_literal(target, &mut show)?;
                        } else {
                            let table = self.table_name(self.req(target, "BaseTableName")?)?;
                            if !special_show_form(&mut show, &table) {
                                show.query = Some(Box::new(describe_query(table)));
                            }
                        }
                    }
                }
            }
        }
        let select = SelectNode {
            select_list: vec![Expr::star()],
            from_table: TableRefKind::Show(show).into(),
            ..SelectNode::default()
        };
        Ok(SelectStatement { node: select.into() })
    }

    fn show_type_of(&self, node: &ParseNode) -> Result<ShowType> {
        Ok(match self.name(node) {
            "SummarizeRule" => ShowType::Summary,
            "ShowRule" => ShowType::Show,
            _ => ShowType::Describe,
        })
    }

    fn describe_literal(&self, node: &ParseNode, show: &mut ShowRef) -> Result<()> {
        let name = Self::text(node);
        if name.is_empty() {
            return Err(parser_error("Expected a table name, but got an empty string"));
        }
        show.name = Some(QualifiedName::new(name));
        Ok(())
    }

    fn base_table_from_qualified(&self, node: &ParseNode) -> Result<QualifiedName> {
        Ok(QualifiedName::from_parts(self.rule_texts(self.alt(node)?)))
    }
}

/// `TrySetSpecialShowForm`: DATABASES, SCHEMAS, TABLES and VARIABLES name pragmas, not tables.
fn special_show_form(show: &mut ShowRef, table: &QualifiedName) -> bool {
    if !table.qualification.is_empty() {
        return false;
    }
    let name = table.name.to_lowercase();
    if !matches!(name.as_str(), "databases" | "tables" | "schemas" | "variables") {
        return false;
    }
    show.name = Some(QualifiedName::new(format!("\"{name}\"")));
    show.show_type = ShowType::ShowSpecial;
    true
}

/// `SELECT * FROM <table>`, whose columns are described.
fn describe_query(table: QualifiedName) -> QueryNode {
    SelectNode {
        select_list: vec![Expr::star()],
        from_table: TableRefKind::BaseTable {
            name: table,
            at_clause: None,
        }
        .into(),
        ..SelectNode::default()
    }
    .into()
}
