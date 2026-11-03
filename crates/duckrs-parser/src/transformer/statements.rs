// Ported from duckdb/src/parser/peg/transformer/{transform_insert,transform_update,transform_delete,
// transform_create_table,transform_create_index,transform_drop,transform_transaction,
// transform_checkpoint,transform_call,transform_set,transform_pragma,transform_explain}.cpp
// @ rust-port-baseline
//
//! Statement transformers other than SELECT.

use super::{Transformer, not_implemented, parser_error, visit_expr_mut};
use crate::ast::*;
use crate::matcher::ParseNode;
use duckrs_common::{Error, ErrorKind, Result};

/// `CompressionTypeToString` for the names `USING COMPRESSION` accepts (`EnumUtil` names).
const COMPRESSIONS: &[(&str, &str)] = &[
    ("uncompressed", "Uncompressed"),
    ("constant", "Constant"),
    ("rle", "RLE"),
    ("dictionary", "Dictionary"),
    ("pfor", "PFOR"),
    ("bitpacking", "BitPacking"),
    ("fsst", "FSST"),
    ("chimp", "Chimp"),
    ("patas", "Patas"),
    ("alp", "ALP"),
    ("alprd", "ALPRD"),
    ("zstd", "ZSTD"),
    ("roaring", "Roaring"),
    ("empty", "Empty Validity"),
    ("dict_fsst", "DICT_FSST"),
];

/// Type names that resolve to VARCHAR (`DefaultTypeGenerator`).
const VARCHAR_NAMES: &[&str] = &["varchar", "bpchar", "string", "char", "nvarchar", "text"];

/// Whether `expr` contains a subquery (`ParsedExpression::HasSubquery`).
fn has_subquery(expr: &Expr) -> bool {
    let mut found = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        found |= matches!(e.kind, ExprKind::Subquery { .. });
    });
    found
}

/// A column definition with the constraints written on it (`ConstraintColumnDefinition`).
struct ColumnWithConstraints {
    column: ColumnDefinition,
    not_null: bool,
    /// (is primary key, check mode) of UNIQUE / PRIMARY KEY column constraints.
    unique: Vec<(bool, ConstraintCheckMode)>,
    constraints: Vec<Constraint>,
}

impl Transformer<'_> {
    /// Transforms the chosen alternative of `Statement` (or `ExplainableStatements`).
    pub(super) fn statement_kind(&mut self, node: &ParseNode) -> Result<Statement> {
        match self.name(node) {
            "SelectStatement" | "ExplainSelectStatement" => Ok(Statement::Select(
                self.select_internal(self.req(node, "SelectStatementInternal")?)?,
            )),
            "InsertStatement" => Ok(Statement::Insert(Box::new(self.insert(node)?))),
            "UpdateStatement" => Ok(Statement::Update(Box::new(self.update(node)?))),
            "DeleteStatement" => Ok(Statement::Delete(Box::new(self.delete(node)?))),
            "TruncateStatement" => self.truncate(node),
            "CreateStatement" => self.create(node),
            "DropStatement" => self.drop(node),
            "TransactionStatement" => self.transaction(self.alt(node)?),
            "CheckpointStatement" => {
                let name = if self.sub(node, "CheckpointForce").is_some() {
                    "force_checkpoint"
                } else {
                    "checkpoint"
                };
                let mut function = FunctionExpr::new(name, Vec::new());
                function.name.qualification = vec!["system".into(), "main".into()];
                if let Some(catalog) = self.sub(node, "CatalogName") {
                    function
                        .args
                        .push(FunctionArgument::positional(Expr::constant(Literal::string(
                            Self::text(catalog),
                        ))));
                }
                Ok(Statement::Call(Box::new(function.into())))
            }
            "CallStatement" => {
                let name = QualifiedName::from_parts(self.rule_texts(self.req(node, "QualifiedTableFunction")?));
                let mut function = FunctionExpr::new("", Vec::new());
                function.name = name;
                for arg in self.subs(self.req(node, "TableFunctionArguments")?, "FunctionArgument") {
                    function.args.push(self.function_argument(arg)?);
                }
                Ok(Statement::Call(Box::new(function.into())))
            }
            "SetStatement" => self.set(self.alt(node)?),
            "ResetStatement" => self.reset(self.alt(self.req(node, "ResetTarget")?)?),
            "PragmaStatement" => self.pragma(self.alt(self.req(node, "PragmaAssignOrFunction")?)?),
            "ExplainStatement" => self.explain(node),
            other => Err(not_implemented(format!(
                "the transformer for {other} is not yet ported"
            ))),
        }
    }

    fn returning(&mut self, node: &ParseNode) -> Result<Vec<Expr>> {
        match self.sub(node, "ReturningClause") {
            Some(r) => self.target_list(self.req(r, "TargetList")?),
            None => Ok(Vec::new()),
        }
    }

    fn cte_map(&mut self, node: &ParseNode) -> Result<CteMap> {
        match self.sub(node, "WithClause") {
            Some(with) => self.with_clause(with),
            None => Ok(CteMap::default()),
        }
    }

    fn table_name(&self, node: &ParseNode) -> Result<QualifiedName> {
        match self.base_table_name(node)?.kind {
            TableRefKind::BaseTable { name, .. } => Ok(name),
            _ => Err(Error::new(ErrorKind::Internal, "expected a base table")),
        }
    }

    fn aliased_table(&self, table: &ParseNode, alias: Option<&ParseNode>) -> Result<TableRef> {
        let mut table = self.base_table_name(table)?;
        if let Some(alias) = alias {
            table.alias = Self::text(self.req(alias, "ColId")?);
        }
        Ok(table)
    }

    // -----------------------------------------------------------------------------------------
    // INSERT, UPDATE, DELETE
    // -----------------------------------------------------------------------------------------

    fn insert(&mut self, node: &ParseNode) -> Result<InsertStatement> {
        let cte_map = self.cte_map(node)?;
        let target = self.req(node, "InsertTarget")?;
        let table = self.table_name(self.req(target, "BaseTableName")?)?;
        let alias = match self.sub(target, "InsertAlias") {
            Some(a) => Some(Self::text(self.req(a, "Identifier")?)),
            None => None,
        };
        let by_name = match self.sub(node, "ByNameOrPosition") {
            Some(b) => self.name(self.alt(b)?) == "InsertByNameOrder",
            None => false,
        };
        let columns = match self.sub(node, "InsertColumnList") {
            Some(list) => self.texts(self.req(list, "ColumnList")?, "ColId"),
            None => Vec::new(),
        };
        let values_node = self.alt(self.req(node, "InsertValues")?)?;
        let values = if self.name(values_node) == "DefaultValues" {
            InsertValues::DefaultValues
        } else {
            let select = self.select_internal(self.req(values_node, "SelectStatementInternal")?)?;
            InsertValues::Select(Box::new(select))
        };
        let default_values = values == InsertValues::DefaultValues;
        if !columns.is_empty() && default_values {
            return Err(parser_error(
                "You can not provide both a column list and DEFAULT VALUES, please remove one of the two",
            ));
        }
        if self.sub(node, "OverridingClause").is_some() && default_values {
            return Err(parser_error(
                "OVERRIDING SYSTEM|USER VALUE can not be combined with DEFAULT VALUES, please remove one of the two",
            ));
        }
        let or_action = match self.sub(node, "OrAction") {
            Some(a) if self.name(self.alt(a)?) == "InsertOrReplace" => Some(OnConflictAction::Replace),
            Some(_) => Some(OnConflictAction::Nothing),
            None => None,
        };
        let mut on_conflict = None;
        if let Some(clause) = self.sub(node, "OnConflictClause") {
            if or_action.is_some() {
                return Err(parser_error(
                    "You can not provide both OR REPLACE|IGNORE and an ON CONFLICT clause, please remove the first if \
                     you want to have more granular control",
                ));
            }
            on_conflict = Some(self.on_conflict(clause)?);
        } else if let Some(action) = or_action {
            on_conflict = Some(OnConflictInfo {
                action,
                indexed_columns: Vec::new(),
                condition: None,
                set_info: None,
            });
        }
        let table_alias = if on_conflict.is_some() { alias } else { None };
        Ok(InsertStatement {
            cte_map,
            table,
            table_alias,
            by_name,
            columns,
            values,
            on_conflict,
            returning: self.returning(node)?,
        })
    }

    fn on_conflict(&mut self, node: &ParseNode) -> Result<OnConflictInfo> {
        let action = self.alt(self.req(node, "OnConflictAction")?)?;
        let mut info = if self.name(action) == "OnConflictUpdate" {
            let mut set_info = self.update_set_list(self.req(action, "UpdateSetClauseList")?)?;
            set_info.condition = self.opt_expr(action, "WhereClause")?.map(Box::new);
            OnConflictInfo {
                action: OnConflictAction::Update,
                indexed_columns: Vec::new(),
                condition: None,
                set_info: Some(set_info),
            }
        } else {
            OnConflictInfo {
                action: OnConflictAction::Nothing,
                indexed_columns: Vec::new(),
                condition: None,
                set_info: None,
            }
        };
        if let Some(target) = self.sub(node, "OnConflictTarget") {
            let target = self.alt(target)?;
            if self.name(target) == "OnConflictIndexTarget" {
                return Err(not_implemented("ON CONSTRAINT conflict target is not supported yet"));
            }
            info.indexed_columns = self.texts(self.req(target, "ColumnIdList")?, "ColId");
            info.condition = self.opt_expr(target, "WhereClause")?.map(Box::new);
        }
        Ok(info)
    }

    fn update_set_list(&mut self, node: &ParseNode) -> Result<UpdateSetInfo> {
        let mut result = UpdateSetInfo::default();
        for clause in self.subs(node, "UpdateSetClause") {
            let clause = self.alt(clause)?;
            let expr = self.expr(self.req(clause, "Expression")?)?;
            if self.name(clause) == "UpdateSetElement" {
                let target = self.req(clause, "UpdateSetColumnTarget")?;
                if self.sub(target, "DotIdentifier").is_some() {
                    return Err(parser_error("Qualified column names in UPDATE .. SET not supported"));
                }
                result.columns.push(Self::text(self.req(target, "ColumnName")?));
                result.expressions.push(expr);
                continue;
            }
            let columns = self.texts(clause, "ColumnName");
            match expr.kind {
                ExprKind::Function(f) if f.name.name == "row" => {
                    if f.args.len() != columns.len() {
                        return Err(parser_error(format!(
                            "Could not perform assignment, expected {} values, got {}",
                            columns.len(),
                            f.args.len()
                        )));
                    }
                    result.expressions.extend(f.args.into_iter().map(|a| a.expr));
                }
                kind => {
                    let expr = Expr::from(kind);
                    result.expressions.extend(std::iter::repeat_n(expr, columns.len()));
                }
            }
            result.columns.extend(columns);
        }
        Ok(result)
    }

    fn update(&mut self, node: &ParseNode) -> Result<UpdateStatement> {
        let cte_map = self.cte_map(node)?;
        let target = self.alt(self.req(node, "UpdateTarget")?)?;
        let table = self.aliased_table(self.req(target, "BaseTableName")?, self.sub(target, "UpdateAlias"))?;
        let mut set_info = self.update_set_list(self.req(node, "UpdateSetClauseList")?)?;
        let from_table = match self.sub(node, "FromClause") {
            Some(f) => Some(self.transform_from(f)?),
            None => None,
        };
        set_info.condition = self.opt_expr(node, "WhereClause")?.map(Box::new);
        Ok(UpdateStatement {
            cte_map,
            table,
            set_info,
            from_table,
            returning: self.returning(node)?,
        })
    }

    fn delete(&mut self, node: &ParseNode) -> Result<DeleteStatement> {
        let cte_map = self.cte_map(node)?;
        let target = self.req(node, "TargetOptAlias")?;
        let table = self.aliased_table(self.req(target, "BaseTableName")?, self.sub(target, "TargetAlias"))?;
        let mut using = Vec::new();
        if let Some(clause) = self.sub(node, "DeleteUsingClause") {
            for r in self.subs(clause, "TableRef") {
                using.push(self.table_ref(r)?);
            }
        }
        Ok(DeleteStatement {
            cte_map,
            table,
            using,
            condition: self.opt_expr(node, "WhereClause")?.map(Box::new),
            returning: self.returning(node)?,
        })
    }

    fn truncate(&mut self, node: &ParseNode) -> Result<Statement> {
        if let Some(options) = self.sub(node, "TruncateOptions") {
            let options = self.alt(options)?;
            let behavior = if self.name(options) == "TruncateDropBehavior" {
                Some(options)
            } else {
                self.sub(options, "TruncateDropBehavior")
            };
            if let Some(b) = behavior
                && self.cascade(self.req(b, "DropBehavior")?)?
            {
                return Err(not_implemented("TRUNCATE ... CASCADE is not implemented"));
            }
        }
        Ok(Statement::Delete(Box::new(DeleteStatement {
            cte_map: CteMap::default(),
            table: self.base_table_name(self.req(node, "BaseTableName")?)?,
            using: Vec::new(),
            condition: None,
            returning: Vec::new(),
        })))
    }

    // -----------------------------------------------------------------------------------------
    // CREATE TABLE
    // -----------------------------------------------------------------------------------------

    fn create(&mut self, node: &ParseNode) -> Result<Statement> {
        let variation = self.alt(self.req(node, "CreateStatementVariation")?)?;
        if self.name(variation) != "CreateTableStmt" {
            return Err(not_implemented(format!(
                "the transformer for {} is not yet ported",
                self.name(variation)
            )));
        }
        let mut info = self.create_table(variation)?;
        if self.sub(node, "OrReplace").is_some() {
            if info.on_conflict == OnCreateConflict::Ignore {
                return Err(parser_error(
                    "Cannot specify both OR REPLACE and IF NOT EXISTS within single create statement",
                ));
            }
            info.on_conflict = OnCreateConflict::Replace;
        }
        if let Some(temporary) = self.sub(node, "Temporary") {
            if self.name(self.alt(temporary)?) == "Persistent" {
                return Err(parser_error("PERSISTENT is only supported for CREATE SECRET"));
            }
            info.temporary = true;
        }
        Ok(Statement::CreateTable(Box::new(info)))
    }

    fn create_table(&mut self, node: &ParseNode) -> Result<CreateTableInfo> {
        let name = QualifiedName::from_parts(self.rule_texts(self.alt(self.req(node, "QualifiedName")?)?));
        if name.name.is_empty() {
            return Err(parser_error("Empty table name not supported"));
        }
        if let Some(commit) = self.sub(node, "CommitAction")
            && self.name(self.alt(self.req(commit, "PreserveOrDelete")?)?) == "DeleteRows"
        {
            return Err(not_implemented("Only ON COMMIT PRESERVE ROWS is supported"));
        }
        let mut info = CreateTableInfo {
            name,
            on_conflict: if self.sub(node, "IfNotExists").is_some() {
                OnCreateConflict::Ignore
            } else {
                OnCreateConflict::Error
            },
            temporary: false,
            columns: Vec::new(),
            constraints: Vec::new(),
            query: None,
            partition_keys: Vec::new(),
            sort_keys: Vec::new(),
            options: Vec::new(),
        };
        let definition = self.alt(self.req(node, "CreateTableDefinition")?)?;
        if let Some(options) = self.sub(definition, "PartitionSortedOptions") {
            let options = self.alt(options)?;
            if let Some(p) = self.sub(options, "PartitionOptions") {
                info.partition_keys = self.exprs(p, "Expression")?;
            }
            if let Some(s) = self.sub(options, "SortedOptions") {
                info.sort_keys = self.exprs(s, "Expression")?;
            }
        }
        if let Some(with) = self.sub(definition, "WithList") {
            info.options = self.with_list(with)?;
        }
        if self.name(definition) == "CreateTableAs" {
            if let Some(list) = self.sub(definition, "IdentifierList") {
                for name in self.texts(list, "Identifier") {
                    info.columns.push(ColumnDefinition {
                        name,
                        column_type: None,
                        generated: None,
                        default_value: None,
                        compression: None,
                    });
                }
            }
            let Statement::Select(mut query) = self.statement(self.req(definition, "Statement")?)? else {
                return Err(parser_error("CREATE TABLE AS requires a SELECT clause"));
            };
            if let Some(with_data) = self.sub(definition, "WithData")
                && self.name(self.alt(with_data)?) == "WithNoData"
            {
                let zero = Box::new(Expr::constant(Literal::integer(0)));
                let existing = query
                    .node
                    .modifiers
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m, ResultModifier::Limit { .. }));
                match existing {
                    Some(ResultModifier::Limit { limit, offset, .. }) => {
                        *limit = Some(zero);
                        *offset = None;
                    }
                    _ => query.node.modifiers.push(ResultModifier::Limit {
                        limit: Some(zero),
                        offset: None,
                        percent: false,
                    }),
                }
            }
            info.query = Some(Box::new(query));
            return Ok(info);
        }
        let elements = match self.sub(definition, "CreateTableColumnList") {
            Some(list) => self.subs(list, "CreateTableColumnElement"),
            None => Vec::new(),
        };
        for element in elements {
            let element = self.alt(element)?;
            if self.name(element) == "CreateTableConstraint" {
                let constraint = self.top_level_constraint(self.req(element, "TopLevelConstraint")?)?;
                info.constraints.push(constraint);
                continue;
            }
            let column = self.column_definition(self.req(element, "ColumnDefinition")?)?;
            let index = info.columns.len();
            info.constraints.extend(column.constraints);
            if column.not_null {
                info.constraints.push(Constraint::NotNull(index));
            }
            for (primary_key, check_mode) in column.unique {
                info.constraints.push(Constraint::Unique {
                    index: Some(index),
                    columns: vec![column.column.name.clone()],
                    primary_key,
                    check_mode,
                });
            }
            info.columns.push(column.column);
        }
        if info.columns.is_empty() {
            return Err(parser_error("Table must have at least one column!"));
        }
        Ok(info)
    }

    fn with_list(&mut self, node: &ParseNode) -> Result<Vec<(String, Expr)>> {
        let options = self.alt(self.req(node, "RelOptionOrOids")?)?;
        if self.name(options) == "Oids" {
            return Err(not_implemented("OIDS for index are not yet implemented."));
        }
        let mut result: Vec<(String, Expr)> = Vec::new();
        for option in self.subs(options, "RelOption") {
            let name_node = self.alt(self.req(option, "RelOptionName")?)?;
            let name = if self.name(name_node) == "DottedColLabel" {
                let mut parts = vec![Self::text(self.req(name_node, "ColLabel")?)];
                for label in self.subs(name_node, "DotColLabel") {
                    parts.push(Self::text(self.req(label, "ColLabel")?));
                }
                parts.join(".")
            } else {
                Self::text(name_node)
            };
            let value = match self.sub(option, "RelOptionArgumentOpt") {
                Some(arg) => {
                    let def = self.alt(self.req(arg, "DefArg")?)?;
                    match self.name(def) {
                        "DefArgNull" | "NoneLiteral" => Expr::constant(Literal::null()),
                        "DefArgStringLiteral" | "DefArgKeyword" => Expr::constant(Literal::string(Self::text(def))),
                        "NumberLiteral" => Expr::constant(Literal::number(Self::text(def))),
                        _ => self.expr(def)?,
                    }
                }
                None => Expr::constant(Literal::null()),
            };
            if result.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                return Err(parser_error(format!("Duplicate option \"{name}\" in WITH clause")));
            }
            result.push((name, value));
        }
        Ok(result)
    }

    fn check_mode(&self, node: &ParseNode) -> Result<ConstraintCheckMode> {
        Ok(match self.sub(node, "ConstraintTiming") {
            Some(t) if self.name(self.alt(t)?) == "ImmediateConstraint" => ConstraintCheckMode::Immediate,
            Some(_) => ConstraintCheckMode::Deferred,
            None => ConstraintCheckMode::Default,
        })
    }

    fn check_constraint(&mut self, node: &ParseNode) -> Result<Constraint> {
        let expr = self.expr(self.req(node, "Expression")?)?;
        if has_subquery(&expr) {
            return Err(parser_error("subqueries prohibited in CHECK constraints"));
        }
        Ok(Constraint::Check(Box::new(expr)))
    }

    /// `REFERENCES table (columns) [actions]`, without the referencing columns.
    fn foreign_key(&self, node: &ParseNode) -> Result<Constraint> {
        let name = self.table_name(self.req(node, "BaseTableName")?)?;
        if let Some(actions) = self.sub(node, "KeyActions") {
            let actions = self.alt(actions)?;
            for action in ["UpdateAction", "DeleteAction"] {
                if let Some(a) = self.sub(actions, action) {
                    let kind = self.name(self.alt(self.req(a, "KeyAction")?)?);
                    if !matches!(kind, "NoKeyAction" | "RestrictKeyAction") {
                        return Err(parser_error(
                            "FOREIGN KEY constraints cannot use CASCADE, SET NULL or SET DEFAULT",
                        ));
                    }
                }
            }
        }
        Ok(Constraint::ForeignKey {
            fk_columns: Vec::new(),
            pk_columns: match self.sub(node, "ColumnList") {
                Some(list) => self.texts(list, "ColId"),
                None => Vec::new(),
            },
            schema: name.qualification.last().cloned().unwrap_or_default(),
            table: name.name,
        })
    }

    fn top_level_constraint(&mut self, node: &ParseNode) -> Result<Constraint> {
        let constraint = self.alt(self.req(node, "TopLevelConstraintList")?)?;
        match self.name(constraint) {
            "TopCheckConstraint" => self.check_constraint(self.req(constraint, "CheckConstraint")?),
            "TopForeignKeyConstraint" => {
                let fk_columns = self.texts(self.req(constraint, "ColumnIdList")?, "ColId");
                let Constraint::ForeignKey {
                    pk_columns,
                    schema,
                    table,
                    ..
                } = self.foreign_key(self.req(constraint, "ForeignKeyConstraint")?)?
                else {
                    unreachable!()
                };
                if !pk_columns.is_empty() && pk_columns.len() != fk_columns.len() {
                    return Err(parser_error(
                        "The number of referencing and referenced columns for foreign keys must be the same",
                    ));
                }
                Ok(Constraint::ForeignKey {
                    fk_columns,
                    pk_columns,
                    schema,
                    table,
                })
            }
            primary_or_unique => Ok(Constraint::Unique {
                index: None,
                columns: self.texts(self.req(constraint, "ColumnIdList")?, "ColId"),
                primary_key: primary_or_unique == "TopPrimaryKeyConstraint",
                check_mode: self.check_mode(constraint)?,
            }),
        }
    }

    fn column_definition(&mut self, node: &ParseNode) -> Result<ColumnWithConstraints> {
        let name = Self::text(self.req(node, "Identifier")?);
        let mut column_type = match self.sub(node, "Type") {
            Some(t) => Some(self.type_(t)?),
            None => None,
        };
        let generated = match self.sub(node, "GeneratedColumn") {
            Some(g) => Some(self.generated_column(g)?),
            None => None,
        };
        if column_type.is_none() && generated.is_none() {
            return Err(parser_error(format!(
                "Column {} must have a type or be defined as a GENERATED column.",
                sql_identifier(&name)
            )));
        }
        let mut result = ColumnWithConstraints {
            column: ColumnDefinition {
                name: name.clone(),
                column_type: None,
                generated: None,
                default_value: None,
                compression: None,
            },
            not_null: false,
            unique: Vec::new(),
            constraints: Vec::new(),
        };
        let mut has_collation = false;
        for constraint in self.subs(node, "ColumnConstraint") {
            let constraint = self.alt(constraint)?;
            if self.name(constraint) == "ColumnCollation" {
                if has_collation {
                    return Err(parser_error("multiple COLLATE clauses not allowed"));
                }
                has_collation = true;
                if generated.is_some() {
                    return Err(parser_error("Collations are not supported on generated columns"));
                }
                let dotted = self.req(constraint, "DottedIdentifier")?;
                let mut parts = vec![Self::text(self.req(dotted, "Identifier")?)];
                for label in self.subs(dotted, "DotColLabel") {
                    parts.push(Self::text(self.req(label, "ColLabel")?));
                }
                column_type = Some(apply_collation(column_type, parts.join("."))?);
                continue;
            }
            let element = self.alt(self.req(constraint, "ColumnConstraintElement")?)?;
            match self.name(element) {
                "DefaultValue" => {
                    if result.column.default_value.is_some() {
                        return Err(parser_error("Cannot define a default value twice"));
                    }
                    let expr = self.expr(self.req(element, "ColumnDefaultExpr")?)?;
                    result.column.default_value = Some(Box::new(expr));
                }
                "NotNullConstraint" => {
                    if self.name(self.alt(element)?) == "NotNullColumnConstraint" {
                        result.not_null = true;
                    }
                }
                "UniqueConstraint" => result.unique.push((false, self.check_mode(element)?)),
                "PrimaryKeyConstraint" => result.unique.push((true, self.check_mode(element)?)),
                "ColumnCompression" => {
                    let requested = Self::text(self.req(element, "ColIdOrString")?).to_lowercase();
                    if requested == "auto" {
                        return Err(parser_error(
                            "Unrecognized option for column compression, expected none, uncompressed, rle, \
                             dictionary, pfor, bitpacking, fsst, chimp, patas, zstd, alp, alprd or roaring",
                        ));
                    }
                    let name = COMPRESSIONS
                        .iter()
                        .find(|(n, _)| *n == requested)
                        .map(|(_, display)| *display)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Conversion,
                                format!("Enum value: '{requested}' not found in CompressionType"),
                            )
                        })?;
                    result.column.compression = Some(name);
                }
                "ForeignKeyConstraint" => {
                    let Constraint::ForeignKey {
                        pk_columns,
                        schema,
                        table,
                        ..
                    } = self.foreign_key(element)?
                    else {
                        unreachable!()
                    };
                    if !pk_columns.is_empty() && pk_columns.len() != 1 {
                        return Err(parser_error(
                            "The number of referencing and referenced columns for foreign keys must be the same",
                        ));
                    }
                    result.constraints.push(Constraint::ForeignKey {
                        fk_columns: vec![name.clone()],
                        pk_columns,
                        schema,
                        table,
                    });
                }
                _ => result.constraints.push(self.check_constraint(element)?),
            }
        }
        if let Some(mut expr) = generated {
            if has_subquery(&expr) {
                return Err(parser_error(format!(
                    "Expression of generated column {name} contains a subquery, which isn't allowed"
                )));
            }
            if let Some(ty) = &column_type {
                expr = Expr::cast(expr, ty.clone());
            }
            if result.column.default_value.is_some() {
                return Err(parser_error("Not allowed to set default on a generated column"));
            }
            result.column.generated = Some(Box::new(expr));
        }
        result.column.column_type = column_type;
        Ok(result)
    }

    fn generated_column(&mut self, node: &ParseNode) -> Result<Expr> {
        if let Some(g) = self.sub(node, "Generated")
            && self.name(self.alt(g)?) == "GeneratedByDefault"
        {
            return Err(parser_error(
                "for a generated column, GENERATED ALWAYS must be specified",
            ));
        }
        if let Some(t) = self.sub(node, "GeneratedColumnType")
            && self.name(self.alt(t)?) == "StoredGeneratedColumn"
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Can not create a STORED generated column!",
            ));
        }
        let expr = self.expr(self.req(node, "Expression")?)?;
        let mut qualified = false;
        visit_expr_mut(&mut expr.clone(), &mut |e| {
            qualified |= matches!(&e.kind, ExprKind::ColumnRef(names) if names.len() > 1);
        });
        if qualified {
            return Err(parser_error(
                "Qualified (tbl.name) column references are not allowed inside of generated column expressions",
            ));
        }
        Ok(expr)
    }

    // -----------------------------------------------------------------------------------------
    // DROP
    // -----------------------------------------------------------------------------------------

    fn cascade(&self, node: &ParseNode) -> Result<bool> {
        Ok(self.name(self.alt(node)?) == "CascadeDropBehavior")
    }

    fn drop(&mut self, node: &ParseNode) -> Result<Statement> {
        let entry = self.alt(self.req(node, "DropEntries")?)?;
        let mut info = DropInfo {
            entry_type: "TABLE",
            name: QualifiedName::default(),
            if_exists: self.sub(entry, "IfExists").is_some(),
            cascade: false,
            trigger_table: None,
            secret_storage: String::new(),
            secret_persist: None,
        };
        match self.name(entry) {
            "DropTable" => {
                info.entry_type = match self.name(self.alt(self.req(entry, "TableOrView")?)?) {
                    "CommentView" => "VIEW",
                    "MaterializedViewEntry" => return Err(not_implemented("Cannot drop MATERIALIZED VIEW yet")),
                    _ => "TABLE",
                };
                info.name = self.table_name(single(self.subs(entry, "BaseTableName"))?)?;
            }
            "DropTableFunction" => {
                info.entry_type = "MACRO TABLE";
                let name = single(self.subs(entry, "QualifiedTableFunction"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(name));
            }
            "DropFunction" => {
                info.entry_type = "MACRO";
                let name = single(self.subs(entry, "FunctionIdentifier"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(self.alt(name)?));
            }
            "DropSchema" => {
                info.entry_type = "SCHEMA";
                let name = single(self.subs(entry, "QualifiedName"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(self.alt(name)?));
            }
            "DropIndex" => {
                info.entry_type = "INDEX";
                let name = single(self.subs(entry, "QualifiedIndexName"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(self.alt(name)?));
            }
            "DropSequence" => {
                info.entry_type = "SEQUENCE";
                let name = single(self.subs(entry, "QualifiedSequenceName"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(name));
            }
            "DropType" => {
                info.entry_type = "TYPE";
                let name = single(self.subs(entry, "QualifiedTypeName"))?;
                info.name = QualifiedName::from_parts(self.rule_texts(self.alt(name)?));
            }
            "DropSecret" => {
                info.entry_type = "SECRET";
                info.name = QualifiedName::new(Self::text(self.req(entry, "SecretName")?));
                if let Some(t) = self.sub(entry, "Temporary") {
                    info.secret_persist = Some(if self.name(self.alt(t)?) == "Persistent" {
                        "PERSISTENT"
                    } else {
                        "TEMPORARY"
                    });
                }
                if let Some(storage) = self.sub(entry, "DropSecretStorage") {
                    if info.secret_persist == Some("TEMPORARY") {
                        return Err(parser_error(
                            "Can not combine TEMPORARY with specifying a storage for drop secret",
                        ));
                    }
                    info.secret_storage = Self::text(self.req(storage, "Identifier")?);
                }
            }
            "DropTrigger" => {
                info.entry_type = "TRIGGER";
                info.name = QualifiedName::new(Self::text(self.req(entry, "TriggerName")?));
                info.trigger_table = Some(Box::new(self.base_table_name(self.req(entry, "BaseTableName")?)?));
            }
            _ => return Err(not_implemented("Cannot drop collation yet")),
        }
        if let Some(behavior) = self.sub(node, "DropBehavior") {
            if info.entry_type == "SECRET" {
                return Err(parser_error("CASCADE and RESTRICT are not supported for DROP SECRET"));
            }
            info.cascade = self.cascade(behavior)?;
        }
        Ok(Statement::Drop(Box::new(info)))
    }

    // -----------------------------------------------------------------------------------------
    // Transactions, SET, RESET, PRAGMA, EXPLAIN
    // -----------------------------------------------------------------------------------------

    fn transaction(&self, node: &ParseNode) -> Result<Statement> {
        let transaction_type = match self.name(node) {
            "BeginTransaction" => TransactionType::Begin,
            "CommitTransaction" => TransactionType::Commit,
            _ => TransactionType::Rollback,
        };
        let modifier = match self.sub(node, "ReadOrWrite") {
            Some(rw) if self.name(self.alt(self.req(rw, "ReadOnlyOrReadWrite")?)?) == "ReadOnly" => {
                TransactionModifier::ReadOnly
            }
            Some(_) => TransactionModifier::ReadWrite,
            None => TransactionModifier::Default,
        };
        Ok(Statement::Transaction {
            transaction_type,
            modifier,
        })
    }

    fn setting_scope(&self, node: &ParseNode) -> Result<SetScope> {
        Ok(match self.sub(node, "SettingScope") {
            Some(s) => match self.name(self.alt(s)?) {
                "LocalScope" => SetScope::Local,
                "SessionScope" => SetScope::Session,
                _ => SetScope::Global,
            },
            None => SetScope::Automatic,
        })
    }

    /// `SetVariableOrSetting`: the name and scope of a variable or setting.
    fn setting(&self, node: &ParseNode) -> Result<(String, SetScope)> {
        let inner = self.alt(node)?;
        if self.name(inner) == "SetVariable" {
            return Ok((Self::text(self.req(inner, "Identifier")?), SetScope::Variable));
        }
        Ok((Self::text(self.req(inner, "SettingName")?), self.setting_scope(inner)?))
    }

    fn set(&mut self, node: &ParseNode) -> Result<Statement> {
        let set_local = || not_implemented("SET LOCAL is not implemented.");
        if self.name(node) == "VariableAssignment" {
            return Ok(Statement::SetVariable {
                name: Self::text(self.req(node, "Identifier")?),
                value: Box::new(self.expr(self.req(node, "Expression")?)?),
                scope: SetScope::Variable,
            });
        }
        let assignment = self.alt(self.req(node, "SetAssignmentOrTimeZone")?)?;
        match self.name(assignment) {
            "SetSchema" => {
                let scope = self.setting_scope(assignment)?;
                if scope == SetScope::Local {
                    return Err(set_local());
                }
                let schema = Self::text(self.req(assignment, "StringLiteral")?);
                Ok(Statement::SetVariable {
                    name: "schema".into(),
                    value: Box::new(Expr::constant(Literal::string(schema))),
                    scope,
                })
            }
            "SetTimeZone" => {
                let scope = self.setting_scope(assignment)?;
                if scope == SetScope::Local {
                    return Err(set_local());
                }
                let zone = self.alt(self.req(assignment, "ZoneValue")?)?;
                let value = match self.name(zone) {
                    "ZoneLocal" | "ZoneDefault" => {
                        return Ok(Statement::ResetVariable {
                            name: "timezone".into(),
                            scope,
                        });
                    }
                    "ZoneIntervalWithPrecision" | "ZoneIntervalWithInterval" => {
                        let text = Self::text(self.req(zone, "StringLiteral")?);
                        Expr::cast(Expr::constant(Literal::string(text)), CastType::Builtin("INTERVAL"))
                    }
                    "NumberLiteral" => Expr::constant(Literal::number(Self::text(zone))),
                    _ => Expr::constant(Literal::string(Self::text(zone))),
                };
                Ok(Statement::SetVariable {
                    name: "timezone".into(),
                    value: Box::new(value),
                    scope,
                })
            }
            _ => {
                let (name, scope) = self.setting(self.req(assignment, "SetVariableOrSetting")?)?;
                if scope == SetScope::Local {
                    return Err(set_local());
                }
                let list = self.req(self.req(assignment, "SetAssignment")?, "VariableList")?;
                let mut values = self.exprs(list, "Expression")?;
                if values.len() > 1 {
                    return Err(parser_error("SET can only contain a single value"));
                }
                let value = match values.pop().expect("SET has a value") {
                    Expr {
                        kind: ExprKind::ColumnRef(names),
                        ..
                    } => {
                        let text = if names.len() > 1 {
                            Expr::column(names).to_string()
                        } else {
                            names[0].clone()
                        };
                        Expr::constant(Literal::string(text))
                    }
                    Expr {
                        kind: ExprKind::Default,
                        ..
                    } => return Ok(Statement::ResetVariable { name, scope }),
                    other => other,
                };
                Ok(Statement::SetVariable {
                    name,
                    value: Box::new(value),
                    scope,
                })
            }
        }
    }

    fn reset(&self, node: &ParseNode) -> Result<Statement> {
        let (name, scope) = if self.name(node) == "ResetTimeZone" {
            ("timezone".to_string(), self.setting_scope(node)?)
        } else {
            self.setting(self.req(node, "SetVariableOrSetting")?)?
        };
        if scope == SetScope::Local {
            return Err(not_implemented("RESET LOCAL is not implemented."));
        }
        Ok(Statement::ResetVariable { name, scope })
    }

    fn pragma(&mut self, node: &ParseNode) -> Result<Statement> {
        let colref_to_string = |expr: Expr| -> Expr {
            match &expr.kind {
                ExprKind::ColumnRef(names) if names.len() == 1 => Expr::constant(Literal::string(names[0].clone())),
                ExprKind::ColumnRef(_) => Expr::constant(Literal::string(expr.to_string())),
                _ => expr,
            }
        };
        if self.name(node) == "PragmaAssign" {
            let name = Self::text(self.req(node, "SettingName")?);
            let mut values = self.exprs(self.req(node, "VariableList")?, "Expression")?;
            if values.len() != 1 {
                return Err(parser_error(
                    "PRAGMA statement with assignment should contain exactly one parameter",
                ));
            }
            let value = colref_to_string(values.pop().unwrap());
            if name.eq_ignore_ascii_case("table_info") {
                return Ok(Statement::Pragma(PragmaInfo {
                    name,
                    parameters: vec![value],
                    named_parameters: Vec::new(),
                }));
            }
            return Ok(Statement::SetVariable {
                name,
                value: Box::new(value),
                scope: SetScope::Automatic,
            });
        }
        let mut info = PragmaInfo {
            name: Self::text(self.req(node, "PragmaName")?),
            parameters: Vec::new(),
            named_parameters: Vec::new(),
        };
        if let Some(params) = self.sub(node, "PragmaParameters") {
            for param in self.exprs(params, "Expression")? {
                match param.kind {
                    ExprKind::Comparison {
                        op: ComparisonOp::Equal,
                        left,
                        right,
                    } => {
                        let ExprKind::ColumnRef(names) = left.kind else {
                            return Err(parser_error("Named parameter requires a column reference on the LHS"));
                        };
                        info.named_parameters
                            .push((names.last().cloned().unwrap_or_default(), *right));
                    }
                    kind => info.parameters.push(colref_to_string(Expr::from(kind))),
                }
            }
        }
        Ok(Statement::Pragma(info))
    }

    fn explain(&mut self, node: &ParseNode) -> Result<Statement> {
        let mut explain_type = if self.sub(node, "AnalyzeKeyword").is_some() {
            ExplainType::Analyze
        } else {
            ExplainType::Standard
        };
        let mut format: Option<String> = None;
        let mut sql = false;
        if let Some(list) = self.sub(node, "ExplainOptionList") {
            for option in self.subs(list, "ExplainOption") {
                let name = Self::text(self.req(option, "ExplainOptionName")?).to_lowercase();
                let value = self.opt_expr(option, "Expression")?;
                match name.as_str() {
                    "format" => {
                        if format.is_some() {
                            return Err(invalid_input("FORMAT can not be provided more than once"));
                        }
                        format = Some(match value.map(|v| v.kind) {
                            Some(ExprKind::Constant(Literal {
                                kind: LiteralKind::String,
                                text,
                            })) => text.to_lowercase(),
                            Some(ExprKind::ColumnRef(names)) => {
                                names.last().cloned().unwrap_or_default().to_lowercase()
                            }
                            Some(ExprKind::Constant(_)) => {
                                return Err(invalid_input("Expected a string as argument to FORMAT"));
                            }
                            Some(ExprKind::Default) => "default".into(),
                            _ => {
                                return Err(invalid_input("FORMAT requires a single format name, e.g. FORMAT json"));
                            }
                        });
                    }
                    "sql" => {
                        if sql || value.is_some() {
                            return Err(invalid_input("SQL must be provided once without arguments"));
                        }
                        sql = true;
                    }
                    "analyze" => explain_type = ExplainType::Analyze,
                    other => return Err(not_implemented(format!("Unimplemented explain type: {other}"))),
                }
            }
        }
        if sql {
            if format.is_some() || explain_type == ExplainType::Analyze {
                return Err(invalid_input("EXPLAIN (SQL) cannot be combined with ANALYZE or FORMAT"));
            }
            explain_type = ExplainType::Sql;
        }
        let statement = self.statement_kind(self.alt(self.req(node, "ExplainableStatements")?)?)?;
        Ok(Statement::Explain {
            statement: Box::new(statement),
            explain_type,
            format: format.unwrap_or_else(|| "default".into()),
        })
    }
}

/// DROP takes one object at a time.
fn single(names: Vec<&ParseNode>) -> Result<&ParseNode> {
    if names.len() > 1 {
        return Err(not_implemented("Can only drop one object at a time"));
    }
    Ok(names[0])
}

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// `ApplyColumnCollation`: a collation turns the VARCHAR type into `VARCHAR(collation)`.
fn apply_collation(column_type: Option<CastType>, collation: String) -> Result<CastType> {
    let Some(CastType::Parsed(ty)) = column_type else {
        return Err(parser_error("Specify the VARCHAR type to set a collation"));
    };
    let ExprKind::Type(type_expr) = &ty.kind else {
        return Err(Error::new(ErrorKind::Internal, "Expected a type expression"));
    };
    if !type_expr.name.qualification.is_empty()
        || !VARCHAR_NAMES
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&type_expr.name.name))
    {
        return Err(parser_error("Only VARCHAR columns can have collations!"));
    }
    let child = Expr::constant(Literal::string(collation)).with_alias("collation");
    Ok(CastType::Parsed(Box::new(
        ExprKind::Type(TypeExpr {
            name: QualifiedName::new("VARCHAR"),
            children: vec![child],
        })
        .into(),
    )))
}
