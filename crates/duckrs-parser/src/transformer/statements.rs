// Ported from duckdb/src/parser/peg/transformer/{transform_insert,transform_update,transform_delete,
// transform_create_table,transform_create_index,transform_drop,transform_transaction,
// transform_checkpoint,transform_call,transform_set,transform_pragma,transform_explain}.cpp
// @ rust-port-baseline
//
//! Statement transformers other than SELECT.

mod alter;
mod match_recognize;
mod pivot;
pub(in crate::transformer) use pivot::PivotEntry;
mod utility;

use super::{Transformer, not_implemented, parser_error, to_recursive_cte, visit_expr_mut};
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
            "UseStatement" => self.use_statement(self.alt(self.req(node, "UseTarget")?)?),
            "PrepareStatement" => {
                if self.sub(node, "TypeList").is_some() {
                    return Err(not_implemented(
                        "TypeList for prepared statement has not been implemented.",
                    ));
                }
                let name = Self::text(self.req(node, "ColIdOrString")?);
                let statement = self.statement(self.req(node, "Statement")?)?;
                let kind = match &statement {
                    Statement::Select(_)
                    | Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_)
                    | Statement::Copy(_) => None,
                    Statement::Merge(_) => Some("MERGE_INTO_STATEMENT"),
                    Statement::ExternalResource(_) => Some("EXTERNAL_RESOURCE_STATEMENT"),
                    Statement::Connect(_) => Some("CONNECT_STATEMENT"),
                    Statement::Disconnect => Some("DISCONNECT_STATEMENT"),
                    Statement::UpdateExtensions(_) => Some("UPDATE_EXTENSIONS_STATEMENT"),
                    Statement::CopyDatabase { .. } => Some("COPY_DATABASE_STATEMENT"),
                    Statement::Export { .. } => Some("EXPORT_STATEMENT"),
                    Statement::Attach(_) => Some("ATTACH_STATEMENT"),
                    Statement::Vacuum { .. } => Some("VACUUM_STATEMENT"),
                    Statement::Load(_) => Some("LOAD_STATEMENT"),
                    Statement::Explain { .. } => Some("EXPLAIN_STATEMENT"),
                    Statement::CreateTable(_) | Statement::Create(_) => Some("CREATE_STATEMENT"),
                    Statement::Drop(_) | Statement::Deallocate(_) => Some("DROP_STATEMENT"),
                    Statement::Transaction { .. } => Some("TRANSACTION_STATEMENT"),
                    Statement::Call(_) => Some("CALL_STATEMENT"),
                    Statement::SetVariable { .. } | Statement::ResetVariable { .. } => Some("SET_STATEMENT"),
                    Statement::Pragma(_) => Some("PRAGMA_STATEMENT"),
                    Statement::Prepare { .. } => Some("PREPARE_STATEMENT"),
                    Statement::Execute { .. } => Some("EXECUTE_STATEMENT"),
                    Statement::Detach { .. } => Some("DETACH_STATEMENT"),
                    Statement::Alter(_) => Some("ALTER_STATEMENT"),
                    Statement::Multi(_) => Some("MULTI_STATEMENT"),
                };
                if let Some(kind) = kind {
                    return Err(parser_error(format!("{kind} is not a preparable statement")));
                }
                self.param_count = 0;
                self.named_params.clear();
                self.param_named = None;
                Ok(Statement::Prepare {
                    name,
                    statement: Box::new(statement),
                })
            }
            "ExecuteStatement" => self.execute(node),
            "AlterStatement" => self.alter(node),
            "CommentStatement" => self.comment(node),
            "ExternalResourceStatement" => self.external_resource(node),
            "ConnectStatement" => self.connect(node),
            "DisconnectStatement" => Ok(Statement::Disconnect),
            "UpdateExtensionsStatement" => Ok(Statement::UpdateExtensions(self.texts(node, "Identifier"))),
            "MergeIntoStatement" => Ok(Statement::Merge(Box::new(self.merge(node)?))),
            "CopyStatement" => self.copy(node),
            "ExportStatement" => self.export(node),
            "ImportStatement" => Ok(Statement::Pragma(PragmaInfo {
                name: "import_database".into(),
                parameters: vec![Expr::constant(Literal::string(Self::text(
                    self.req(node, "StringLiteral")?,
                )))],
                named_parameters: Vec::new(),
            })),
            "AttachStatement" => self.attach(node),
            "AnalyzeStatement" => self.analyze(node),
            "VacuumStatement" => self.vacuum(node),
            "LoadStatement" => self.load(node),
            "InstallStatement" => self.install(node),
            "ExtensionRepositoryStatement" => self.extension_repository(node),
            "DeallocateStatement" => Ok(Statement::Deallocate(Self::text(self.req(node, "ColIdOrString")?))),
            "DetachStatement" => Ok(Statement::Detach {
                name: Self::text(self.req(node, "CatalogName")?),
                if_exists: self.sub(node, "IfExists").is_some(),
            }),
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
        let or_replace = self.sub(node, "OrReplace").is_some();
        let persistent = match self.sub(node, "Temporary") {
            Some(t) => self.name(self.alt(t)?) == "Persistent",
            None => false,
        };
        let temporary = self.sub(node, "Temporary").is_some() && !persistent;
        if persistent && self.name(variation) != "CreateSecretStmt" {
            if self.name(variation) == "CreateTriggerStmt" {
                // the trigger body is transformed first in DuckDB, so its errors win
                self.create_trigger(variation)?;
            }
            return Err(parser_error("PERSISTENT is only supported for CREATE SECRET"));
        }
        let apply = |on_conflict: &mut OnCreateConflict| -> Result<()> {
            if or_replace {
                if *on_conflict == OnCreateConflict::Ignore {
                    return Err(parser_error(
                        "Cannot specify both OR REPLACE and IF NOT EXISTS within single create statement",
                    ));
                }
                *on_conflict = OnCreateConflict::Replace;
            }
            Ok(())
        };
        if self.name(variation) == "CreateTableStmt" {
            let mut info = self.create_table(variation)?;
            apply(&mut info.on_conflict)?;
            info.temporary = temporary;
            return Ok(Statement::CreateTable(Box::new(info)));
        }
        let kind = match self.name(variation) {
            "CreateViewStmt" => self.create_view(variation)?,
            "CreateSchemaStmt" => self.create_schema(variation)?,
            "CreateSequenceStmt" => self.create_sequence(variation)?,
            "CreateIndexStmt" => {
                if temporary {
                    return Err(parser_error("Temporary indexes are not supported"));
                }
                self.create_index(variation)?
            }
            "CreateMacroStmt" => self.create_macro(variation)?,
            "CreateTypeStmt" => self.create_type(variation)?,
            "CreateTriggerStmt" => {
                if temporary {
                    return Err(parser_error("Temporary triggers are not supported"));
                }
                self.create_trigger(variation)?
            }
            "CreateSecretStmt" => self.create_secret(variation, persistent)?,
            other => {
                return Err(not_implemented(format!(
                    "the transformer for {other} is not yet ported"
                )));
            }
        };
        let name = match &kind {
            CreateKind::Index { .. } => self.index_name(variation)?,
            CreateKind::Trigger { .. } => QualifiedName::new(Self::text(self.req(variation, "TriggerName")?)),
            CreateKind::Secret { secret_type, .. } => match self.sub(variation, "SecretName") {
                Some(n) => QualifiedName::new(Self::text(n)),
                None => {
                    let Some(ty) = secret_type else {
                        return Err(parser_error(
                            "Failed to create secret - secret must have a type defined",
                        ));
                    };
                    let value = match &ty.kind {
                        ExprKind::Constant(l) if l.kind != LiteralKind::Null => l.text.clone(),
                        ExprKind::ColumnRef(names) => names.last().cloned().unwrap_or_default(),
                        _ => {
                            return Err(invalid_input(
                                "Can not combine a non-constant expression for the secret type with a default-named \
                                 secret. Either provide an explicit secret name or use a constant expression for the \
                                 secret type.",
                            ));
                        }
                    };
                    QualifiedName::new(format!("__default_{}", value.to_lowercase()))
                }
            },
            _ => self.qualified_name(self.req(variation, "QualifiedName")?)?,
        };
        let mut info = CreateInfo {
            name,
            on_conflict: if self.sub(variation, "IfNotExists").is_some() {
                OnCreateConflict::Ignore
            } else {
                OnCreateConflict::Error
            },
            temporary,
            kind,
        };
        apply(&mut info.on_conflict)?;
        if let CreateKind::View { aliases, query, .. } = &mut info.kind
            && self.sub(variation, "CreateRecursive").is_some()
        {
            let node = std::mem::replace(&mut query.node, SelectNode::default().into());
            **query = recursive_view(&info.name, aliases, node)?;
        }
        Ok(Statement::Create(Box::new(info)))
    }

    fn create_trigger(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let timing = match self.name(self.alt(self.req(node, "TriggerTiming")?)?) {
            "TriggerBefore" => "BEFORE",
            "TriggerInsteadOf" => "INSTEAD OF",
            _ => "AFTER",
        };
        let event_node = self.alt(self.req(node, "TriggerEvent")?)?;
        let (event, columns) = match self.name(event_node) {
            "TriggerEventInsert" => ("INSERT", Vec::new()),
            "TriggerEventDelete" => ("DELETE", Vec::new()),
            "TriggerEventUpdateOf" => (
                "UPDATE",
                self.texts(self.req(event_node, "TriggerColumnList")?, "ColId"),
            ),
            _ => ("UPDATE", Vec::new()),
        };
        let table = self.base_table_name(self.req(node, "BaseTableName")?)?;
        let (mut referencing_new, mut referencing_old) = (String::new(), String::new());
        if let Some(clause) = self.sub(node, "ReferencingClause") {
            for item in self.subs(clause, "ReferencingItem") {
                let item = self.alt(item)?;
                let alias = Self::text(self.req(item, "ColId")?);
                let (slot, which) = if self.name(item) == "ReferencingNewTableAs" {
                    (&mut referencing_new, "NEW")
                } else {
                    (&mut referencing_old, "OLD")
                };
                if !slot.is_empty() {
                    return Err(parser_error(format!(
                        "{which} TABLE cannot be specified multiple times in REFERENCING clause"
                    )));
                }
                *slot = alias;
            }
            if !referencing_new.is_empty() && referencing_new.eq_ignore_ascii_case(&referencing_old) {
                return Err(parser_error("REFERENCING aliases must be distinct"));
            }
        }
        let for_each = match self.sub(node, "ForEachClause") {
            Some(f) if self.name(self.alt(f)?) == "ForEachRow" => "ROW",
            _ => "STATEMENT",
        };
        let action: QueryNode = match self.statement_kind(self.alt(self.req(node, "TriggerBody")?)?)? {
            Statement::Insert(s) => QueryNodeKind::Insert(s).into(),
            Statement::Update(s) => QueryNodeKind::Update(s).into(),
            Statement::Delete(s) => QueryNodeKind::Delete(s).into(),
            Statement::Merge(s) => QueryNodeKind::Merge(s).into(),
            _ => {
                return Err(parser_error(
                    "Trigger body must be an INSERT, UPDATE, or DELETE statement",
                ));
            }
        };
        Ok(CreateKind::Trigger {
            timing,
            event,
            columns,
            table: Box::new(table),
            referencing_new,
            referencing_old,
            for_each,
            action: Box::new(action),
        })
    }

    /// `QualifiedName <- CatalogReservedSchemaIdentifier / SchemaReservedIdentifierOrStringLiteral / ...`.
    fn qualified_name(&self, node: &ParseNode) -> Result<QualifiedName> {
        Ok(QualifiedName::from_parts(self.rule_texts(self.alt(node)?)))
    }

    /// An index lives in the schema of its table.
    fn index_name(&self, node: &ParseNode) -> Result<QualifiedName> {
        let Some(name) = self.sub(node, "IndexName") else {
            return Err(not_implemented(
                "Please provide an index name, e.g., CREATE INDEX my_name ...",
            ));
        };
        let table = self.table_name(self.req(node, "BaseTableName")?)?;
        Ok(QualifiedName {
            qualification: table.qualification,
            name: Self::text(name),
        })
    }

    fn create_view(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let recursive = self.sub(node, "CreateRecursive").is_some();
        let aliases = match self.sub(node, "InsertColumnList") {
            Some(list) => self.texts(self.req(list, "ColumnList")?, "ColId"),
            None => Vec::new(),
        };
        if recursive && aliases.is_empty() {
            return Err(parser_error("CREATE RECURSIVE VIEW requires a column list"));
        }
        let mut defer_binding = false;
        if let Some(with) = self.sub(node, "WithList") {
            for (name, value) in self.with_list(with)? {
                if !name.eq_ignore_ascii_case("defer_binding") {
                    return Err(parser_error(
                        "Only DEFER_BINDING is currently supported as option for CREATE VIEW",
                    ));
                }
                let ExprKind::Constant(literal) = value.kind else {
                    return Err(invalid_input("Defer binding option must be a constant value"));
                };
                match literal.kind {
                    LiteralKind::Null => defer_binding = true,
                    LiteralKind::Boolean => defer_binding |= literal.text == "true",
                    _ => return Err(invalid_input("Defer binding option must be a boolean")),
                }
            }
        }
        let query = self.select_internal(self.req(node, "SelectStatementInternal")?)?;
        self.pivot_entry_check("view")?;
        Ok(CreateKind::View {
            secure: self.sub(node, "CreateSecure").is_some(),
            aliases,
            defer_binding,
            query: Box::new(query),
        })
    }

    fn create_schema(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let options = match self.sub(node, "WithOptionList") {
            Some(with) => self.rel_options(self.req(with, "RelOptionList")?)?,
            None => Vec::new(),
        };
        Ok(CreateKind::Schema { options })
    }

    /// A constant sequence option value; a negated constant is folded.
    fn sequence_value(&mut self, node: &ParseNode, option: &str, integer: bool) -> Result<Literal> {
        let mut expr = self.expr(self.req(node, "Expression")?)?;
        if let ExprKind::Function(f) = &expr.kind
            && integer
        {
            if f.name.name != "-" {
                return Err(invalid_input(&format!(
                    "Expected a minus function instead of {}",
                    f.name.name
                )));
            }
            match f.args.first().map(|a| &a.expr.kind) {
                Some(ExprKind::Constant(literal)) if literal.is_numeric() => {
                    expr = Expr::constant(literal.negate());
                }
                _ => return Err(invalid_input("Expected constant expression as child of minus function")),
            }
        }
        let ExprKind::Constant(literal) = expr.kind else {
            return Err(parser_error("Expected constant expression."));
        };
        if literal.kind != LiteralKind::Null && literal.kind != LiteralKind::Integer {
            return Err(parser_error(format!(
                "Expected an integer argument for {option}, got {literal}"
            )));
        }
        Ok(literal)
    }

    fn create_sequence(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let mut options: Vec<(&'static str, Literal)> = Vec::new();
        for option in self.subs(node, "SequenceOption") {
            let option = self.alt(option)?;
            let min_or_max = |s: &Self, n: &ParseNode| -> Result<&'static str> {
                Ok(if s.name(s.alt(s.req(n, "SeqMinOrMax")?)?) == "MinValue" {
                    "minvalue"
                } else {
                    "maxvalue"
                })
            };
            let entry = match self.name(option) {
                "SeqSetCycle" => ("cycle", Literal::boolean(self.name(self.alt(option)?) == "SeqCycle")),
                "SeqSetIncrement" => ("increment", self.sequence_value(option, "INCREMENT", true)?),
                "SeqSetMinMax" => {
                    let which = min_or_max(self, option)?;
                    let label = if which == "minvalue" { "MINVALUE" } else { "MAXVALUE" };
                    (which, self.sequence_value(option, label, true)?)
                }
                "SeqNoMinMax" => {
                    let which = min_or_max(self, option)?;
                    (
                        if which == "minvalue" {
                            "nominvalue"
                        } else {
                            "nomaxvalue"
                        },
                        Literal::boolean(true),
                    )
                }
                "SeqStartWith" => ("start", self.sequence_value(option, "START", false)?),
                _ => ("owned", Literal::null()),
            };
            if options.iter().any(|(n, _)| *n == entry.0) {
                let mut name = entry.0.to_string();
                name[..1].make_ascii_uppercase();
                return Err(parser_error(format!("{name} should be passed at most once")));
            }
            options.push(entry);
        }
        let no_min = options.iter().any(|(n, _)| *n == "nominvalue");
        let no_max = options.iter().any(|(n, _)| *n == "nomaxvalue");
        if no_min && options.iter().any(|(n, _)| *n == "minvalue") {
            return Err(parser_error("Minvalue should be passed at most once"));
        }
        if no_max && options.iter().any(|(n, _)| *n == "maxvalue") {
            return Err(parser_error("Maxvalue should be passed at most once"));
        }
        let int = |literal: &Literal, what: &str| -> Result<i64> {
            if literal.kind == LiteralKind::Null {
                return Err(parser_error(format!("{what} must not be NULL")));
            }
            literal
                .try_get_i64()
                .ok_or_else(|| Error::new(ErrorKind::Conversion, format!("Could not convert {literal} to INT64")))
        };
        let (mut increment, mut min_value, mut max_value) = (1i64, 1i64, i64::MAX);
        let (mut start_value, mut cycle) = (None, false);
        let (mut min_set, mut max_set) = (false, false);
        for (name, literal) in &options {
            match *name {
                "increment" => {
                    increment = int(literal, "INCREMENT")?;
                    if increment == 0 {
                        return Err(parser_error("Increment must not be zero"));
                    }
                    if increment < 0 {
                        if !min_set {
                            min_value = i64::MIN;
                        }
                        if !max_set {
                            max_value = -1;
                        }
                    } else {
                        if !max_set {
                            max_value = i64::MAX;
                        }
                        if !min_set {
                            min_value = 1;
                        }
                    }
                }
                "minvalue" => {
                    min_value = int(literal, "MINVALUE")?;
                    min_set = true;
                }
                "maxvalue" => {
                    max_value = int(literal, "MAXVALUE")?;
                    max_set = true;
                }
                "start" => start_value = Some(int(literal, "START value")?),
                "cycle" => cycle = literal.text == "true",
                "nominvalue" | "nomaxvalue" => {}
                other => {
                    return Err(parser_error(format!(
                        "Unrecognized option \"{other}\" for CREATE SEQUENCE"
                    )));
                }
            }
        }
        let start_value = start_value.unwrap_or(if increment < 0 { max_value } else { min_value });
        if max_value <= min_value {
            return Err(parser_error(format!(
                "MINVALUE ({min_value}) must be less than MAXVALUE ({max_value})"
            )));
        }
        // a non cycling sequence can start outside its range, the exhausted state EXPORT DATABASE
        // writes; a cycling one wraps and never exhausts, so its range still bounds its START
        if cycle && start_value < min_value {
            return Err(parser_error(format!(
                "START value ({start_value}) cannot be less than MINVALUE ({min_value})"
            )));
        }
        if cycle && start_value > max_value {
            return Err(parser_error(format!(
                "START value ({start_value}) cannot be greater than MAXVALUE ({max_value})"
            )));
        }
        Ok(CreateKind::Sequence {
            increment,
            min_value,
            max_value,
            start_value,
            cycle,
        })
    }

    fn create_index(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let table = self.table_name(self.req(node, "BaseTableName")?)?.name;
        let index_type = self
            .sub(node, "IndexType")
            .map(|t| Self::text(self.req(t, "Identifier").unwrap_or(t)));
        let mut expressions = Vec::new();
        for element in self.subs(node, "IndexElement") {
            let mut expr = self.expr(self.req(element, "Expression")?)?;
            if index_type.is_none()
                && let ExprKind::ColumnRef(names) = &mut expr.kind
                && names.len() == 1
            {
                names.insert(0, table.clone());
            }
            if matches!(expr.kind, ExprKind::Collate { .. }) {
                return Err(not_implemented("Index with collation not supported yet!"));
            }
            expressions.push(expr);
        }
        if self.sub(node, "WhereClause").is_some() {
            return Err(not_implemented("Creating partial indexes is not supported currently"));
        }
        let mut options = Vec::new();
        if let Some(with) = self.sub(node, "WithList") {
            for (name, value) in self.with_list(with)? {
                let ExprKind::Constant(literal) = value.kind else {
                    return Err(invalid_input("Create index option must be a constant value"));
                };
                options.push((name, literal));
            }
        }
        Ok(CreateKind::Index {
            table,
            unique: self.sub(node, "UniqueIndex").is_some(),
            index_type: index_type.unwrap_or_else(|| "ART".into()),
            expressions,
            options,
        })
    }

    fn create_macro(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let mut macros = Vec::new();
        for definition in self.subs(node, "MacroDefinition") {
            let body_node = self.alt(self.req(definition, "MacroDefinitionBody")?)?;
            let body = if self.name(body_node) == "TableMacroDefinition" {
                let query = self.select_internal(self.req(body_node, "SelectStatementInternal")?)?;
                MacroBody::Table(Box::new(query.node))
            } else {
                MacroBody::Scalar(Box::new(self.expr(self.req(body_node, "Expression")?)?))
            };
            let return_type = match self.sub(definition, "ReturnsType") {
                Some(r) => {
                    if matches!(body, MacroBody::Table(_)) {
                        return Err(parser_error("RETURNS types are not supported for table macros"));
                    }
                    Some(self.type_(self.req(r, "Type")?)?)
                }
                None => None,
            };
            let mut parameters: Vec<MacroParameter> = Vec::new();
            if let Some(list) = self.sub(definition, "MacroParameters") {
                for parameter in self.subs(list, "MacroParameter") {
                    let parameter = self.alt(parameter)?;
                    let name = Self::text(self.req(parameter, "TypeFuncName")?);
                    if parameters.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
                        return Err(parser_error(format!("Duplicate parameter {name} in macro definition")));
                    }
                    let parameter_type = match self.sub(parameter, "Type") {
                        Some(t) => Some(self.type_(t)?),
                        None => None,
                    };
                    let default = match self.sub(parameter, "Expression") {
                        Some(e) => Some(self.expr(e)?.with_alias(name.clone())),
                        None => {
                            if parameters.iter().any(|p| p.default.is_some()) {
                                return Err(parser_error(
                                    "Parameter without a default follows parameter with a default",
                                ));
                            }
                            None
                        }
                    };
                    parameters.push(MacroParameter {
                        name,
                        parameter_type,
                        default,
                    });
                }
            }
            macros.push(MacroFunction {
                parameters,
                return_type,
                body,
            });
        }
        let table_macro = matches!(macros[0].body, MacroBody::Table(_));
        self.pivot_entry_check("macro")?;
        if macros
            .iter()
            .any(|m| matches!(m.body, MacroBody::Table(_)) != table_macro)
        {
            return Err(parser_error("Cannot mix table and scalar macro function definitions"));
        }
        Ok(CreateKind::Macro { table_macro, macros })
    }

    fn create_type(&mut self, node: &ParseNode) -> Result<CreateKind> {
        let ty = self.alt(self.req(node, "CreateType")?)?;
        Ok(CreateKind::Type(match self.name(ty) {
            "EnumSelectType" => CreateTypeKind::EnumQuery(Box::new(
                self.select_internal(self.req(ty, "SelectStatementInternal")?)?,
            )),
            "EnumStringLiteralList" => {
                let values = self.texts(ty, "StringLiteral");
                let bounds = self.subs(ty, "ArrayBounds");
                if bounds.is_empty() {
                    CreateTypeKind::Enum(values)
                } else {
                    // an array of the ENUM is a parsed type over the ENUM spelled as a type
                    let mut enum_type: Expr = ExprKind::Type(TypeExpr {
                        name: QualifiedName::new("ENUM"),
                        children: values.into_iter().map(|v| Expr::constant(Literal::string(v))).collect(),
                    })
                    .into();
                    for bound in bounds {
                        enum_type = self.wrap_array_bound(enum_type, self.alt(bound)?)?;
                    }
                    CreateTypeKind::Alias(CastType::Parsed(Box::new(enum_type)))
                }
            }
            _ => CreateTypeKind::Alias(self.type_(self.req(ty, "Type")?)?),
        }))
    }

    /// USE becomes `SET schema`.
    fn use_statement(&self, node: &ParseNode) -> Result<Statement> {
        let value = if self.name(node) == "UseTargetCatalogSchema" {
            if self.sub(node, "DotIdentifier").is_some() {
                return Err(parser_error("Expected \"USE database\" or \"USE database.schema\""));
            }
            format!(
                "{}.{}",
                sql_identifier(&Self::text(self.req(node, "CatalogName")?)),
                sql_identifier(&Self::text(self.req(node, "ReservedSchemaName")?))
            )
        } else {
            sql_identifier(&Self::text(node))
        };
        Ok(Statement::SetVariable {
            name: "schema".into(),
            value: Box::new(Expr::constant(Literal::string(value))),
            scope: SetScope::Automatic,
        })
    }

    fn execute(&mut self, node: &ParseNode) -> Result<Statement> {
        let name = Self::text(self.req(node, "ColIdOrString")?);
        let mut values: Vec<(String, Expr)> = Vec::new();
        let mut positional = 0;
        if let Some(args) = self.sub(node, "TableFunctionArguments") {
            for (i, arg) in self.subs(args, "FunctionArgument").into_iter().enumerate() {
                let arg = self.function_argument(arg)?;
                if !is_scalar(&arg.expr) {
                    return Err(invalid_input(
                        "Only scalar parameters, named parameters or NULL supported for EXECUTE",
                    ));
                }
                let mixing =
                    || not_implemented("Mixing named parameters and positional parameters is not supported yet");
                let key = if arg.name.is_empty() {
                    if positional != i {
                        return Err(mixing());
                    }
                    positional += 1;
                    positional.to_string()
                } else {
                    if positional != 0 {
                        return Err(mixing());
                    }
                    arg.name
                };
                let mut expr = arg.expr;
                expr.alias.clear();
                values.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
                values.push((key, expr));
            }
        }
        Ok(Statement::Execute { name, values })
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
        self.rel_options(options)
    }

    /// `RelOptionList <- Parens(List(RelOption))`.
    fn rel_options(&mut self, options: &ParseNode) -> Result<Vec<(String, Expr)>> {
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
                let name_node = self.req(option, "ExplainOptionName")?;
                let name = if self.sub(name_node, "AnalyzeKeyword").is_some() {
                    "analyze".to_string()
                } else {
                    Self::text(name_node).to_lowercase()
                };
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
        // the statement is transformed first, as its PIVOTs are what the check is about
        let statement = self.statement_kind(self.alt(self.req(node, "ExplainableStatements")?)?)?;
        if sql {
            if format.is_some() || explain_type == ExplainType::Analyze {
                return Err(invalid_input("EXPLAIN (SQL) cannot be combined with ANALYZE or FORMAT"));
            }
            self.pivot_entry_check("EXPLAIN (SQL) statement")?;
            explain_type = ExplainType::Sql;
        }
        Ok(Statement::Explain {
            statement: Box::new(statement),
            explain_type,
            format: format.unwrap_or_else(|| "default".into()),
        })
    }
}

/// `ParsedExpression::IsScalar`: no column, positional or DEFAULT references and no subqueries.
fn is_scalar(expr: &Expr) -> bool {
    let mut scalar = true;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        scalar &= !matches!(
            e.kind,
            ExprKind::ColumnRef(_) | ExprKind::Positional(_) | ExprKind::Default | ExprKind::Subquery { .. }
        );
    });
    scalar
}

/// `ConvertToRecursiveView`: the query becomes a recursive CTE named after the view, selected from.
fn recursive_view(name: &QualifiedName, aliases: &[String], node: QueryNode) -> Result<SelectStatement> {
    let query = to_recursive_cte(node, &name.name, aliases, &[])?;
    let mut cte_map = CteMap::default();
    cte_map.0.push((
        name.name.clone(),
        CteInfo {
            aliases: aliases.to_vec(),
            query: Box::new(query),
            key_targets: Vec::new(),
            materialized: CteMaterialize::Default,
        },
    ));
    let select = SelectNode {
        select_list: aliases.iter().map(|a| Expr::column(vec![a.clone()])).collect(),
        from_table: TableRefKind::BaseTable {
            name: name.clone(),
            at_clause: None,
        }
        .into(),
        ..SelectNode::default()
    };
    let mut node: QueryNode = select.into();
    node.cte_map = cte_map;
    Ok(SelectStatement { node })
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
