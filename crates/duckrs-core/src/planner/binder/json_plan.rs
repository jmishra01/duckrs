// Ported from duckdb/extension/json/json_functions/json_serialize_plan.cpp @ rust-port-baseline
//
//! `json_serialize_plan`: the logical plans of the statements of a SQL text, as JSON. Planning needs the
//! catalog, which function callbacks do not have, so a constant SQL text is planned while binding the call,
//! and the call folds to the document.

use super::{Binder, ParameterValues, binder_error};
use crate::execution::expression_executor::evaluate_scalar;
use crate::function::json::{Json, json_type};
use crate::function::json_serialize::{error_document, write_document};
use crate::optimizer::optimize;
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::BoundExpr;
use crate::storage::serialization::plan::{self, PlanJsonOptions};
use duckrs_common::types::LogicalTypeId;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;
use std::sync::Arc;

pub(super) const NAME: &str = "json_serialize_plan";

/// The options after the SQL text, in the order of the function's parameters.
const OPTIONS: [&str; 5] = ["skip_null", "skip_empty", "skip_default", "format", "optimize"];

impl Binder<'_> {
    /// `JsonSerializePlanBind`, and `JsonSerializePlanFunction` of a constant SQL text.
    pub(super) fn bind_json_serialize_plan(&mut self, function: &ast::FunctionExpr) -> Result<BoundExpr> {
        let mut children = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        if children.is_empty() {
            return Err(binder_error(format!("{NAME} takes at least one argument")));
        }
        if children[0].return_type.id() != LogicalTypeId::Varchar {
            return Err(Error::new(
                ErrorKind::InvalidType,
                format!("{NAME} first argument must be a VARCHAR"),
            ));
        }
        let mut flags = [false; OPTIONS.len()];
        for (position, (child, argument)) in children.iter().zip(&function.args).enumerate().skip(1) {
            let name = if argument.name.is_empty() {
                OPTIONS.get(position - 1).copied().unwrap_or_default()
            } else {
                argument.name.as_str()
            };
            let Some(at) = OPTIONS.iter().position(|option| option.eq_ignore_ascii_case(name)) else {
                return Err(binder_error(format!("{NAME}: Unknown argument {name}")));
            };
            if child.return_type.id() != LogicalTypeId::Boolean {
                return Err(binder_error(format!("{NAME}: '{name}' argument must be a boolean")));
            }
            flags[at] = matches!(evaluate_scalar(child)?.data(), ValueData::Bool(true));
        }
        let [skip_null, skip_empty, skip_default, format, optimize] = flags;
        let sql = children.swap_remove(0);
        if !sql.is_foldable() {
            return Err(crate::unported(format!("{NAME} of a SQL text that is not a constant")));
        }
        let sql = evaluate_scalar(&sql)?;
        if sql.is_null() {
            return Ok(BoundExpr::constant(Value::null(json_type())));
        }
        let options = PlanJsonOptions {
            skip_null,
            skip_empty,
            skip_default,
        };
        let document = match self.serialize_plans(&sql.try_to_string()?, optimize, options) {
            Ok(plans) => Json::Object(vec![
                ("error".to_string(), Json::Bool(false)),
                ("plans".to_string(), Json::Array(plans)),
            ]),
            Err(error) => error_document(&error),
        };
        Ok(BoundExpr::constant(
            Value::varchar(write_document(&document, format)).reinterpret(&json_type())?,
        ))
    }

    /// The plan of each statement of `sql`, as a fresh statement of the session plans it (`Planner::CreatePlan`).
    fn serialize_plans(&self, sql: &str, optimize_plan: bool, options: PlanJsonOptions) -> Result<Vec<Json>> {
        let statements = duckrs_parser::parse_ast_with(sql, &self.options.parser)?;
        let mut plans = Vec::with_capacity(statements.len());
        for statement in &statements {
            let mut binder = Binder::with_options(self.catalog, self.options.clone());
            binder.session = self.session.clone();
            // parameters bind as NULLs, to tell that the statement has some
            let parameters = Arc::new(ParameterValues::default());
            binder.parameters = Some(parameters.clone());
            if let ast::Statement::Create(info) = statement
                && let ast::CreateKind::Macro { .. } = &info.kind
            {
                let bind = |ty: &ast::CastType| binder.bind_type(ty);
                plans.push(plan::serialize_create_macro(info, &bind, options)?);
                continue;
            }
            let bound = binder.bind(statement)?;
            if !parameters.seen.lock().is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "{NAME}: statement cannot be planned, parameters need to have a known value to serialize the plan"
                    ),
                ));
            }
            let mut logical = bound.plan;
            if optimize_plan {
                logical = optimize(logical, self.options.optimizer);
                crate::optimizer::join_order::take_error()?;
            }
            // what DuckDB's projection pushdown leaves of a scan of a collection
            let chunk_columns = if optimize_plan {
                plan::chunk_get_columns(&mut logical)
            } else {
                Default::default()
            };
            resolve_column_bindings(&mut logical)?;
            plans.push(plan::serialize_plan(&logical, chunk_columns, options)?);
        }
        Ok(plans)
    }
}
