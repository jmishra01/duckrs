// Ported from duckdb/src/planner/binder/tableref/bind_showref.cpp,
// duckdb/src/planner/binder/statement/bind_summarize.cpp, the column information of
// duckdb/src/function/table/system/pragma_table_info.cpp and the catalog listings of
// duckdb/src/function/table/system/duckdb_{tables,views,columns,schemas,databases}.cpp
// @ rust-port-baseline
//
//! SHOW, DESCRIBE, SUMMARIZE and the `duckdb_*` listings of the catalog. They are rows computed
//! while binding, as `LogicalColumnDataGet` holds in C++.

use super::table_functions::parse_select;
use super::{Binder, ShowBehavior, binder_error};
use crate::catalog::{SchemaCatalogEntry, TEMP_SCHEMA, TableCatalogEntry, TableConstraint, TableOrView};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{ErrorKind, Result};
use duckrs_parser::ast::{
    Expr, QualifiedName, QueryNode, SelectNode, SelectStatement, ShowRef, ShowType, TableRef, TableRefKind,
    sql_identifier,
};
use std::sync::Arc;

fn text(value: &str) -> Value {
    Value::varchar(value)
}

fn null_text() -> Value {
    Value::null(LogicalType::VARCHAR)
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `CheckConstraints`: whether a column is in a primary key and in a UNIQUE.
fn column_keys(table: &TableCatalogEntry, column: usize) -> (bool, bool) {
    let (mut primary, mut unique) = (false, false);
    for constraint in &table.constraints {
        if let TableConstraint::Unique { columns, primary_key } = constraint
            && columns.contains(&column)
        {
            if *primary_key {
                primary = true;
            } else {
                unique = true;
            }
        }
    }
    (primary, unique)
}

/// The row of `DESCRIBE` for a column of a table, `name` taking the place of the column's name.
fn column_info(table: &TableCatalogEntry, name: &str, column: &str) -> Vec<Value> {
    let logical = table.logical_columns();
    let found = logical
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(column))
        .expect("a column of the table");
    let (primary, unique) = table
        .column_index(column)
        .map_or((false, false), |i| column_keys(table, i));
    let not_null = found.definition.is_some_and(|d| d.not_null) || primary;
    let key = if primary {
        text("PRI")
    } else if unique {
        text("UNI")
    } else {
        null_text()
    };
    let default = match (found.generated, found.definition.and_then(|d| d.default_text.as_ref())) {
        (Some(expr), _) => text(&expr.to_string()),
        (None, Some(default)) => text(default),
        _ => null_text(),
    };
    vec![
        text(name),
        text(&found.ty.to_string()),
        text(if not_null { "NO" } else { "YES" }),
        key,
        default,
        null_text(),
    ]
}

fn plain_column_info(name: &str, ty: &LogicalType) -> Vec<Value> {
    vec![
        text(name),
        text(&ty.to_string()),
        text("YES"),
        null_text(),
        null_text(),
        null_text(),
    ]
}

/// `FindBaseTableColumn`: the column of a base table that an output column of `op` passes on.
fn base_table_column(op: &LogicalOperator, binding: ColumnBinding) -> Option<(Arc<TableCatalogEntry>, usize)> {
    match &op.kind {
        LogicalOperatorKind::Get {
            table,
            table_index,
            column_ids,
            ..
        } => {
            if *table_index != binding.table_index {
                return None;
            }
            let column = column_ids[binding.column_index];
            (column != COLUMN_IDENTIFIER_ROW_ID).then(|| (table.clone(), column))
        }
        LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } => {
            if *table_index != binding.table_index {
                return None;
            }
            match &expressions[binding.column_index].kind {
                BoundExprKind::ColumnRef { binding, .. } => base_table_column(&op.children[0], *binding),
                _ => None,
            }
        }
        LogicalOperatorKind::Limit { .. }
        | LogicalOperatorKind::Order { .. }
        | LogicalOperatorKind::TopN { .. }
        | LogicalOperatorKind::Distinct { .. }
        | LogicalOperatorKind::Filter { .. }
        | LogicalOperatorKind::Sample { .. }
        | LogicalOperatorKind::LimitPercent { .. }
        | LogicalOperatorKind::Join { .. }
        | LogicalOperatorKind::CrossProduct => op.children.iter().find_map(|c| base_table_column(c, binding)),
        _ => None,
    }
}

const DESCRIBE_NAMES: [&str; 6] = ["column_name", "column_type", "null", "key", "default", "extra"];

const SYSTEM_TABLES: [&str; 5] = [
    "duckdb_databases",
    "duckdb_schemas",
    "duckdb_tables",
    "duckdb_views",
    "duckdb_columns",
];

/// A table of the listing, with the ids the listings share.
struct ListedEntry {
    schema: Arc<SchemaCatalogEntry>,
    schema_oid: i64,
    oid: i64,
    entry: TableOrView,
}

impl Binder<'_> {
    /// Rows known while binding as a table called `default_alias`.
    pub(super) fn bind_rows(
        &mut self,
        table_ref: &TableRef,
        default_alias: &str,
        names: Vec<String>,
        types: Vec<LogicalType>,
        rows: Vec<Vec<Value>>,
    ) -> Result<LogicalOperator> {
        let alias = if table_ref.alias.is_empty() {
            default_alias.to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            generated: Vec::new(),
        });
        let expressions = rows
            .into_iter()
            .map(|row| row.into_iter().map(BoundExpr::constant).collect())
            .collect();
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ExpressionGet {
                table_index,
                types,
                expressions,
            },
            vec![],
        ))
    }

    fn bind_describe_rows(&mut self, table_ref: &TableRef, rows: Vec<Vec<Value>>) -> Result<LogicalOperator> {
        let names = DESCRIBE_NAMES.iter().map(|n| n.to_string()).collect();
        self.bind_rows(table_ref, "__show_select", names, vec![LogicalType::VARCHAR; 6], rows)
    }

    /// `Binder::BindDescribeQuery`.
    fn bind_describe_query(&mut self, table_ref: &TableRef, query: &QueryNode) -> Result<LogicalOperator> {
        let mut binder = self.child_binder();
        let bound = binder.bind_query_node(query);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let bindings = bound.plan.column_bindings();
        let mut rows = Vec::with_capacity(bound.names.len());
        for (i, name) in bound.names.iter().enumerate() {
            rows.push(match base_table_column(&bound.plan, bindings[i]) {
                Some((table, column)) => column_info(&table, name, &table.columns[column].name),
                None => plain_column_info(name, &bound.types[i]),
            });
        }
        self.bind_describe_rows(table_ref, rows)
    }

    /// The columns of a table or view as `DESCRIBE name` shows them.
    fn describe_entry(&mut self, name: &QualifiedName) -> Result<Vec<Vec<Value>>> {
        if name.qualification.is_empty()
            && let Some(index) = self.ctes.iter().rposition(|(n, ..)| n.eq_ignore_ascii_case(&name.name))
        {
            let reference = TableRef::from(TableRefKind::BaseTable {
                name: name.clone(),
                at_clause: None,
            });
            let mut binder = self.child_binder();
            let _ = index;
            binder.bind_table_ref(&reference)?;
            self.next_table_index = binder.next_table_index;
            let binding = &binder.bind_context.bindings[0];
            return Ok(binding
                .names
                .iter()
                .zip(&binding.types)
                .map(|(n, t)| plain_column_info(n, t))
                .collect());
        }
        let entry = self
            .catalog
            .get_entry(super::schema_of(name)?, self.search_schema.as_deref(), &name.name)?;
        Ok(match entry {
            TableOrView::Table(table) => table
                .logical_columns()
                .iter()
                .map(|c| column_info(&table, c.name, c.name))
                .collect(),
            TableOrView::View(view) => {
                let (names, types) = self.view_columns(&view)?;
                names.iter().zip(&types).map(|(n, t)| plain_column_info(n, t)).collect()
            }
        })
    }

    fn view_columns(&mut self, view: &crate::catalog::ViewCatalogEntry) -> Result<(Vec<String>, Vec<LogicalType>)> {
        let reference = TableRef::from(TableRefKind::BaseTable {
            name: QualifiedName {
                qualification: vec![view.schema.clone()],
                ..QualifiedName::new(view.name.clone())
            },
            at_clause: None,
        });
        let mut binder = self.child_binder();
        binder.bind_table_ref(&reference)?;
        self.next_table_index = binder.next_table_index;
        let binding = &binder.bind_context.bindings[0];
        Ok((binding.names.clone(), binding.types.clone()))
    }

    /// `PragmaShowTables`: the names on the search path.
    fn show_tables(&mut self, table_ref: &TableRef, schema: Option<&str>) -> Result<LogicalOperator> {
        let mut names: Vec<String> = Vec::new();
        for listed in self.listed_entries() {
            let wanted = match schema {
                Some(schema) => listed.schema.name.eq_ignore_ascii_case(schema),
                None => true,
            };
            if wanted {
                names.push(listed.entry.name().to_string());
            }
        }
        names.sort();
        let rows = names.into_iter().map(|n| vec![Value::varchar(n)]).collect();
        self.bind_rows(
            table_ref,
            "__show_select",
            vec!["name".to_string()],
            vec![LogicalType::VARCHAR],
            rows,
        )
    }

    /// `PragmaShowTablesExpanded`.
    fn show_all_tables(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        let text_list = LogicalType::list(LogicalType::VARCHAR);
        let mut rows = Vec::new();
        for listed in self.listed_entries() {
            let (names, types) = match &listed.entry {
                TableOrView::Table(table) => {
                    let columns = table.logical_columns();
                    (
                        columns.iter().map(|c| c.name.to_string()).collect(),
                        columns.iter().map(|c| c.ty.clone()).collect(),
                    )
                }
                TableOrView::View(view) => self.view_columns(view)?,
            };
            let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
            rows.push(vec![
                text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                text(if temporary { "main" } else { &listed.schema.name }),
                text(listed.entry.name()),
                Value::list(LogicalType::VARCHAR, names.into_iter().map(Value::varchar).collect())?,
                Value::list(
                    LogicalType::VARCHAR,
                    types.iter().map(|t| Value::varchar(t.to_string())).collect(),
                )?,
                Value::boolean(temporary),
            ]);
        }
        rows.sort_by_key(|row| {
            row[..3]
                .iter()
                .map(|v| v.try_to_string().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        let names = [
            "database",
            "schema",
            "name",
            "column_names",
            "column_types",
            "temporary",
        ];
        let types = vec![
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            text_list.clone(),
            text_list,
            LogicalType::BOOLEAN,
        ];
        self.bind_rows(
            table_ref,
            "__show_select",
            names.iter().map(|n| n.to_string()).collect(),
            types,
            rows,
        )
    }

    /// `Binder::TryBindShowSetting`.
    fn bind_show_setting(&mut self, table_ref: &TableRef, name: &str) -> Result<Option<LogicalOperator>> {
        let Some(session) = self.session.clone() else {
            return Ok(None);
        };
        let Ok(value) = (session.setting)(name) else {
            return Ok(None);
        };
        let ty = value.logical_type().clone();
        Ok(Some(self.bind_rows(
            table_ref,
            "__show_setting",
            vec![name.to_string()],
            vec![ty],
            vec![vec![value]],
        )?))
    }

    fn bind_describe(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        if let Some(query) = &show.query {
            return self.bind_describe_query(table_ref, query);
        }
        if show.show_type == ShowType::ShowFrom {
            let schema = if show.schema.is_empty() {
                show.catalog.clone()
            } else {
                show.schema.clone()
            };
            if self.catalog.get_schema(&schema).is_none() {
                return Err(duckrs_common::Error::new(
                    ErrorKind::Catalog,
                    format!(
                        "SHOW TABLES FROM: No catalog + schema named \"{}.{schema}\" found.",
                        self.catalog.name
                    ),
                ));
            }
            return self.show_tables(table_ref, Some(&schema));
        }
        let Some(name) = &show.name else {
            return self.show_all_tables(table_ref);
        };
        match name.name.as_str() {
            "\"databases\"" => {
                let name = self.catalog.name.clone();
                self.bind_rows(
                    table_ref,
                    "__show_select",
                    vec!["database_name".to_string()],
                    vec![LogicalType::VARCHAR],
                    vec![vec![Value::varchar(name)]],
                )
            }
            "\"schemas\"" => {
                let select = "SELECT database_name, schema_name, schema_name = current_schema() AS \"current\" \
                              FROM duckdb_schemas() WHERE NOT internal ORDER BY ALL";
                let select = parse_select(select, "Expected a single SELECT statement")?;
                self.bind_rewritten(&select, table_ref, "__show_select")
            }
            "\"tables\"" => self.show_tables(table_ref, None),
            "__show_tables_expanded" => self.show_all_tables(table_ref),
            "\"variables\"" => Err(crate::unported("SHOW VARIABLES")),
            _ => {
                let name = name.clone();
                let rows = self.describe_entry(&name)?;
                self.bind_describe_rows(table_ref, rows)
            }
        }
    }

    /// `Binder::BindSummarize`.
    fn bind_summarize(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        let query: QueryNode = match (&show.query, &show.name) {
            (Some(query), _) => (**query).clone(),
            (None, Some(name)) => SelectNode {
                select_list: vec![Expr::star()],
                from_table: TableRefKind::BaseTable {
                    name: name.clone(),
                    at_clause: None,
                }
                .into(),
                ..SelectNode::default()
            }
            .into(),
            _ => return Err(binder_error("SUMMARIZE requires a table or a query")),
        };
        let select = SelectStatement { node: query };
        let mut binder = self.child_binder();
        let bound = binder.bind_select_statement(&select);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let mut columns: [Vec<String>; 12] = Default::default();
        for (name, ty) in bound.names.iter().zip(&bound.types) {
            let column = sql_identifier(name);
            let type_name = ty.to_string();
            let temporal = type_name.starts_with("DATE") || type_name.starts_with("TIME");
            let numeric = ty.is_numeric();
            let aggregate = |function: &str, modifier: &str| format!("CAST({function}({column}{modifier}) AS VARCHAR)");
            let unless = |condition: bool, value: String| if condition { value } else { "NULL".to_string() };
            columns[0].push(sql_string(name));
            columns[1].push(sql_string(&type_name));
            columns[2].push(aggregate("min", ""));
            columns[3].push(aggregate("max", ""));
            columns[4].push(format!("CAST({} AS BIGINT)", aggregate("approx_count_distinct", "")));
            columns[5].push(unless(numeric || temporal, aggregate("avg", "")));
            columns[6].push(unless(numeric, aggregate("stddev", "")));
            for (slot, quantile) in [(7, "0.25"), (8, "0.50"), (9, "0.75")] {
                let modifier = format!(", CAST({quantile} AS FLOAT)");
                columns[slot].push(unless(numeric || temporal, aggregate("approx_quantile", &modifier)));
            }
            columns[10].push("count_star()".to_string());
            columns[11].push(format!(
                "CAST(CASE WHEN count_star() > 0 THEN (CAST(1 AS DOUBLE) - CAST({} AS DOUBLE) / \
                 CAST(count_star() AS DOUBLE)) * CAST(100 AS DOUBLE) ELSE NULL END AS DECIMAL(9, 2))",
                aggregate("count", "")
            ));
        }
        const ALIASES: [&str; 12] = [
            "column_name",
            "column_type",
            "min",
            "max",
            "approx_unique",
            "avg",
            "std",
            "q25",
            "q50",
            "q75",
            "count",
            "null_percentage",
        ];
        let list: Vec<String> = ALIASES
            .iter()
            .zip(&columns)
            .map(|(alias, values)| format!("unnest(list_value({})) AS {}", values.join(", "), sql_identifier(alias)))
            .collect();
        let mut parsed = parse_select(&format!("SELECT {}", list.join(", ")), "Expected a SELECT")?;
        let mut source = TableRef::subquery(select);
        source.alias = "summarize_tbl".to_string();
        if let duckrs_parser::ast::QueryNodeKind::Select(node) = &mut parsed.node.kind {
            node.from_table = source;
        }
        self.bind_rewritten(&parsed, table_ref, "unnamed_subquery")
    }

    /// `Binder::Bind(ShowRef)`.
    pub(super) fn bind_show_ref(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        match show.show_type {
            ShowType::Summary => self.bind_summarize(table_ref, show),
            ShowType::Show if show.query.is_none() => {
                let name = show.name.as_ref();
                let can_be_setting = name.is_some_and(|n| n.qualification.is_empty());
                if self.options.show_behavior == ShowBehavior::Setting {
                    let setting = name.map(|n| n.to_string()).unwrap_or_default();
                    if can_be_setting
                        && let Some(plan) = self.bind_show_setting(table_ref, &name.expect("checked").name)?
                    {
                        return Ok(plan);
                    }
                    return Err(duckrs_common::Error::new(
                        ErrorKind::Catalog,
                        format!("Setting with name \"{setting}\" does not exist"),
                    ));
                }
                if self.options.show_behavior == ShowBehavior::Table {
                    return self.bind_describe(table_ref, show);
                }
                match self.bind_describe(table_ref, show) {
                    Err(error) if error.kind == ErrorKind::Catalog && can_be_setting => {
                        let setting = name.expect("checked above").name.clone();
                        match self.bind_show_setting(table_ref, &setting)? {
                            Some(plan) => Ok(plan),
                            None => Err(error),
                        }
                    }
                    result => result,
                }
            }
            _ => self.bind_describe(table_ref, show),
        }
    }

    /// `pragma_table_info` and `pragma_show`.
    pub(super) fn bind_pragma_table_info(
        &mut self,
        table_ref: &TableRef,
        function: &str,
        name: &QualifiedName,
    ) -> Result<LogicalOperator> {
        let described = self.describe_entry(name)?;
        if function.eq_ignore_ascii_case("pragma_show") {
            return self.bind_describe_rows(table_ref, described);
        }
        let mut rows = Vec::with_capacity(described.len());
        for (cid, row) in described.into_iter().enumerate() {
            let [column, ty, null, key, default, _] = <[Value; 6]>::try_from(row).expect("six columns");
            let not_null = null.try_to_string()? == "NO";
            let primary = !key.is_null() && key.try_to_string()? == "PRI";
            rows.push(vec![
                Value::integer(cid as i32),
                column,
                ty,
                Value::boolean(not_null),
                default,
                Value::boolean(primary),
            ]);
        }
        let names = ["cid", "name", "type", "notnull", "dflt_value", "pk"];
        let types = vec![
            LogicalType::INTEGER,
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            LogicalType::BOOLEAN,
            LogicalType::VARCHAR,
            LogicalType::BOOLEAN,
        ];
        self.bind_rows(
            table_ref,
            function,
            names.iter().map(|n| n.to_string()).collect(),
            types,
            rows,
        )
    }

    /// Everything in the catalog, with stable ids.
    fn listed_entries(&self) -> Vec<ListedEntry> {
        let mut result = Vec::new();
        let mut oid = 1000;
        for (schema_oid, schema) in self.catalog.schemas().into_iter().enumerate() {
            for entry in schema.tables_and_views() {
                oid += 1;
                result.push(ListedEntry {
                    schema: schema.clone(),
                    schema_oid: schema_oid as i64 + 1,
                    oid,
                    entry,
                });
            }
        }
        result
    }

    /// `pragma_storage_info`: the segments of a table in its file. Rows in memory have none.
    pub(super) fn bind_storage_info(
        &mut self,
        call: &duckrs_parser::ast::FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        // named options say what `segment_info` holds, which isn't filled in here
        let mut positional = call.args.iter().filter(|argument| argument.name.is_empty());
        let (Some(argument), None) = (positional.next(), positional.next()) else {
            return Err(binder_error("pragma_storage_info takes the name of a table"));
        };
        let name = evaluate_scalar(&self.bind_expression(&argument.expr)?)?.try_to_string()?;
        let parts: Vec<&str> = name.split('.').collect();
        let (schema, table) = match parts.as_slice() {
            [table] => (None, *table),
            [schema, table] => (Some(*schema), *table),
            _ => return Err(crate::unported("a table name of more than two parts")),
        };
        let table = self.catalog.get_table(schema, table)?;
        let segments = table.source().map_or(Ok(Vec::new()), |source| source.segments())?;
        let (bigint, varchar, boolean) = (LogicalType::BIGINT, LogicalType::VARCHAR, LogicalType::BOOLEAN);
        let columns = [
            ("row_group_id", bigint.clone()),
            ("column_name", varchar.clone()),
            ("column_id", bigint.clone()),
            ("column_path", varchar.clone()),
            ("segment_id", bigint.clone()),
            ("segment_type", varchar.clone()),
            ("start", bigint.clone()),
            ("count", bigint.clone()),
            ("compression", varchar.clone()),
            ("stats", varchar.clone()),
            ("has_updates", boolean.clone()),
            ("persistent", boolean),
            ("block_id", bigint.clone()),
            ("block_offset", bigint),
            ("segment_info", varchar.clone()),
            ("additional_block_ids", LogicalType::list(LogicalType::BIGINT)),
        ];
        let rows = segments
            .into_iter()
            .map(|s| {
                vec![
                    Value::bigint(s.row_group as i64),
                    Value::varchar(table.columns[s.column].name.clone()),
                    Value::bigint(s.column as i64),
                    Value::varchar(s.path),
                    Value::bigint(s.segment as i64),
                    Value::varchar(s.segment_type),
                    Value::bigint(s.start as i64),
                    Value::bigint(s.count as i64),
                    Value::varchar(s.compression.to_string()),
                    Value::null(LogicalType::VARCHAR),
                    Value::boolean(false),
                    Value::boolean(true),
                    Value::bigint(s.block_id),
                    Value::bigint(s.block_offset as i64),
                    Value::null(LogicalType::VARCHAR),
                    Value::null(LogicalType::list(LogicalType::BIGINT)),
                ]
            })
            .collect();
        let (names, types) = columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, "pragma_storage_info", names, types, rows)
    }

    pub(super) fn is_system_table(name: &str) -> bool {
        SYSTEM_TABLES.iter().any(|t| t.eq_ignore_ascii_case(name))
    }

    /// `duckdb_databases()` and the like.
    pub(super) fn bind_system_table(&mut self, table_ref: &TableRef, name: &str) -> Result<LogicalOperator> {
        let varchar = LogicalType::VARCHAR;
        let bigint = LogicalType::BIGINT;
        let boolean = LogicalType::BOOLEAN;
        let integer = LogicalType::INTEGER;
        let tags = LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR);
        let (columns, rows): (Vec<(&str, LogicalType)>, Vec<Vec<Value>>) = match name.to_lowercase().as_str() {
            "duckdb_databases" => {
                let database = |name: &str, oid: i64, internal: bool| {
                    vec![
                        text(name),
                        Value::bigint(oid),
                        null_text(),
                        null_text(),
                        Value::null(tags.clone()),
                        Value::boolean(internal),
                        text("duckdb"),
                        Value::boolean(false),
                    ]
                };
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("path", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("type", varchar.clone()),
                        ("readonly", boolean.clone()),
                    ],
                    vec![
                        database(&self.catalog.name, 1, false),
                        database("system", 2, true),
                        database("temp", 3, true),
                    ],
                )
            }
            "duckdb_schemas" => {
                let mut rows = Vec::new();
                for (oid, schema) in self.catalog.schemas().iter().enumerate() {
                    let temporary = schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    rows.push(vec![
                        Value::bigint(oid as i64 + 1),
                        text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                        Value::bigint(if temporary { 3 } else { 1 }),
                        text(if temporary { "main" } else { &schema.name }),
                        null_text(),
                        Value::null(tags.clone()),
                        Value::boolean(temporary),
                        null_text(),
                    ]);
                }
                (
                    vec![
                        ("oid", bigint.clone()),
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_tables" => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let TableOrView::Table(table) = &listed.entry else {
                        continue;
                    };
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    let primary = table
                        .constraints
                        .iter()
                        .any(|c| matches!(c, TableConstraint::Unique { primary_key: true, .. }));
                    let checks = table
                        .constraints
                        .iter()
                        .filter(|c| matches!(c, TableConstraint::Check { .. }))
                        .count();
                    let backing_indexes = table
                        .constraints
                        .iter()
                        .filter(|c| matches!(c, TableConstraint::Unique { .. } | TableConstraint::ForeignKey(_)))
                        .count();
                    rows.push(vec![
                        text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                        Value::bigint(if temporary { 3 } else { 1 }),
                        text(if temporary { "main" } else { &listed.schema.name }),
                        Value::bigint(listed.schema_oid),
                        text(&table.name),
                        Value::bigint(listed.oid),
                        null_text(),
                        Value::null(tags.clone()),
                        Value::boolean(false),
                        Value::boolean(temporary),
                        Value::boolean(primary),
                        Value::bigint(table.row_count() as i64),
                        Value::bigint(table.logical_columns().len() as i64),
                        // C++ backs each UNIQUE, PRIMARY KEY and FOREIGN KEY with an index
                        Value::bigint((table.indexes.read().len() + backing_indexes) as i64),
                        Value::bigint(checks as i64),
                        null_text(),
                    ]);
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("table_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("temporary", boolean.clone()),
                        ("has_primary_key", boolean.clone()),
                        ("estimated_size", bigint.clone()),
                        ("column_count", bigint.clone()),
                        ("index_count", bigint.clone()),
                        ("check_constraint_count", bigint.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_views" => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let TableOrView::View(view) = &listed.entry else {
                        continue;
                    };
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    // a view that doesn't bind is still listed, without columns
                    let (names, types) = self.view_columns(view).unwrap_or_default();
                    rows.push(vec![
                        text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                        Value::bigint(if temporary { 3 } else { 1 }),
                        text(if temporary { "main" } else { &listed.schema.name }),
                        Value::bigint(listed.schema_oid),
                        text(&view.name),
                        Value::bigint(listed.oid),
                        null_text(),
                        Value::null(tags.clone()),
                        Value::boolean(false),
                        Value::boolean(temporary),
                        Value::bigint(names.len() as i64),
                        Value::list(LogicalType::VARCHAR, names.into_iter().map(Value::varchar).collect())?,
                        Value::list(
                            LogicalType::VARCHAR,
                            types.iter().map(|t| Value::varchar(t.to_string())).collect(),
                        )?,
                        text(&view.to_sql()),
                    ]);
                }
                let list = LogicalType::list(LogicalType::VARCHAR);
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("view_name", varchar.clone()),
                        ("view_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("temporary", boolean.clone()),
                        ("column_count", bigint.clone()),
                        ("column_names", list.clone()),
                        ("column_types", list),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            _ => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    let table_columns: Vec<(Value, LogicalType, bool, Value, Value, bool)> = match &listed.entry {
                        TableOrView::Table(table) => table
                            .logical_columns()
                            .iter()
                            .map(|c| {
                                let not_null = c.definition.is_some_and(|d| d.not_null);
                                let expression = c.generated.map_or_else(null_text, |e| text(&e.to_string()));
                                let default = c
                                    .definition
                                    .and_then(|d| d.default_text.as_deref())
                                    .map_or_else(|| expression.clone(), text);
                                (
                                    text(c.name),
                                    c.ty.clone(),
                                    not_null,
                                    default,
                                    expression,
                                    c.generated.is_some(),
                                )
                            })
                            .collect(),
                        TableOrView::View(view) => match self.view_columns(view) {
                            Ok((names, types)) => names
                                .into_iter()
                                .zip(types)
                                .map(|(n, t)| (Value::varchar(n), t, false, null_text(), null_text(), false))
                                .collect(),
                            // `ViewColumnHelper`: a view that doesn't bind has a placeholder column
                            Err(_) => vec![(
                                null_text(),
                                LogicalType::INVALID,
                                false,
                                null_text(),
                                null_text(),
                                false,
                            )],
                        },
                    };
                    for (index, (column, ty, not_null, default, expression, generated)) in
                        table_columns.into_iter().enumerate()
                    {
                        rows.push(vec![
                            text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                            Value::bigint(if temporary { 3 } else { 1 }),
                            text(if temporary { "main" } else { &listed.schema.name }),
                            Value::bigint(listed.schema_oid),
                            text(listed.entry.name()),
                            Value::bigint(listed.oid),
                            column,
                            Value::integer(index as i32 + 1),
                            null_text(),
                            Value::boolean(false),
                            default,
                            Value::boolean(!not_null),
                            text(&ty.to_string()),
                            Value::bigint(ty.id() as i64),
                            Value::null(integer.clone()),
                            Value::null(integer.clone()),
                            Value::null(integer.clone()),
                            Value::null(integer.clone()),
                            Value::null(tags.clone()),
                            Value::boolean(generated),
                            expression,
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("table_oid", bigint.clone()),
                        ("column_name", varchar.clone()),
                        ("column_index", integer.clone()),
                        ("comment", varchar.clone()),
                        ("internal", boolean.clone()),
                        ("column_default", varchar.clone()),
                        ("is_nullable", boolean.clone()),
                        ("data_type", varchar.clone()),
                        ("data_type_id", bigint.clone()),
                        ("character_maximum_length", integer.clone()),
                        ("numeric_precision", integer.clone()),
                        ("numeric_precision_radix", integer.clone()),
                        ("numeric_scale", integer.clone()),
                        ("tags", tags.clone()),
                        ("is_generated", boolean.clone()),
                        ("generation_expression", varchar.clone()),
                    ],
                    rows,
                )
            }
        };
        let (names, types): (Vec<String>, Vec<LogicalType>) =
            columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, &name.to_lowercase(), names, types, rows)
    }
}
