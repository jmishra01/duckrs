// Ported from the `BindingMode::EXTRACT_NAMES` and `EXTRACT_QUALIFIED_NAMES` parts of
// duckdb/src/planner/binder/tableref/bind_basetableref.cpp, duckdb/src/planner/binder.cpp (`AddTableName`) and
// duckdb/src/planner/binder/expression/bind_{columnref,function}_expression.cpp @ rust-port-baseline
//
//! Binding a statement only for the names of the tables it reads (`ClientContext::GetTableNames`, which
//! `duckdb_get_table_names` calls): each table, or name the catalog does not have, is noted and bound as a dummy
//! scan, and the columns and scalar functions bind as NULL, so that a statement over tables that are not there
//! still binds. Normal binding does none of this.

use super::{Binder, Binding};
use crate::catalog::TableOrView;
use crate::function::BuiltinFunctions;
use crate::planner::expression::BoundExpr;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use parking_lot::Mutex;
use pintail_common::Result;
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use pintail_parser::ast::{self, Expr, ExprKind, QualifiedName, TableRef};
use std::collections::BTreeSet;
use std::sync::Arc;

/// `BindingMode::EXTRACT_NAMES` (or `EXTRACT_QUALIFIED_NAMES`) and the names found so far
/// (`GlobalBinderState::table_names`), which the binders of a statement share.
#[derive(Debug, Default)]
pub struct ExtractNames {
    qualified: bool,
    names: Mutex<BTreeSet<String>>,
}

impl ExtractNames {
    /// The names found, in order.
    pub fn names(&self) -> Vec<String> {
        self.names.lock().iter().cloned().collect()
    }
}

impl Binder<'_> {
    /// `SetBindingMode(EXTRACT_NAMES)`, or `EXTRACT_QUALIFIED_NAMES` with `qualified`: what binding finds goes to the
    /// result.
    pub fn extract_table_names(&mut self, qualified: bool) -> Arc<ExtractNames> {
        let names = Arc::new(ExtractNames {
            qualified,
            names: Mutex::default(),
        });
        self.extract_names = Some(names.clone());
        names
    }

    /// `Binder::Bind(BaseTableRef &)` in the modes: a table, or a name the catalog does not have, is noted (as written
    /// with `EXTRACT_QUALIFIED_NAMES`) and bound as a scan of one INTEGER column; a view binds as it does otherwise.
    pub(super) fn extract_base_table(
        &mut self,
        table_ref: &TableRef,
        name: &QualifiedName,
        entry: &Result<TableOrView>,
    ) -> Option<LogicalOperator> {
        let extract = self.extract_names.clone()?;
        if matches!(entry, Ok(TableOrView::View(_))) {
            return None;
        }
        let noted = if extract.qualified {
            // `BaseTableRef::ToString`
            let mut text: String = name
                .qualification
                .iter()
                .filter(|q| !q.is_empty())
                .map(|q| format!("{}.", ast::sql_identifier(q)))
                .collect();
            text += &ast::sql_identifier(&name.name);
            if !table_ref.alias.is_empty() {
                text += &format!(" AS {}", ast::sql_identifier(&table_ref.alias));
            }
            text
        } else {
            name.name.clone()
        };
        extract.names.lock().insert(noted);
        let table_index = self.generate_table_index();
        let alias = if table_ref.alias.is_empty() {
            name.name.clone()
        } else {
            table_ref.alias.clone()
        };
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            catalog: None,
            table_index,
            names: vec![format!("__dummy_col{table_index}")],
            types: vec![LogicalType::INTEGER],
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Some(LogicalOperator::new(
            LogicalOperatorKind::DummyScan { table_index },
            vec![],
        ))
    }

    /// `ExpressionBinder::BindExpression` in the modes: a column is NULL, and so is a call of a scalar function once
    /// its arguments are bound. `None` outside the modes, and for what binds as it does otherwise.
    pub(super) fn extract_names_expression(&mut self, expr: &Expr) -> Result<Option<BoundExpr>> {
        if self.extract_names.is_none() {
            return Ok(None);
        }
        let null = || BoundExpr::constant(Value::null(LogicalType::SQLNULL));
        match &expr.kind {
            ExprKind::ColumnRef(_) => Ok(Some(null())),
            ExprKind::Function(function)
                if BuiltinFunctions::get().aggregate(&function.name.name).is_none()
                    && self.user_aggregate(&function.name.name).is_none() =>
            {
                for argument in &function.args {
                    if !matches!(argument.expr.kind, ExprKind::Lambda { .. }) {
                        self.bind_expression(&argument.expr)?;
                    }
                }
                Ok(Some(null()))
            }
            _ => Ok(None),
        }
    }
}
