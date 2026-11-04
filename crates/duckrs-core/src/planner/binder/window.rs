// Ported from duckdb/src/planner/binder/expression/bind_window_expression.cpp @ rust-port-baseline
//
//! Binds window expressions. They run after the grouping, in a `LogicalOperatorKind::Window`, and
//! what's left of them in the expression is a column of that operator.

use super::expression::checked_cast;
use super::{Binder, binder_error};
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::function::combine_types::force_max_logical_type;
use crate::function::generic::is_generic_function;
use crate::planner::expression::{BoundExpr, BoundWindow, ColumnBinding, WindowFunction};
use duckrs_common::sort_key::{OrderModifiers, OrderType};
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    Expr, FunctionArgument, FunctionExpr, OrderByNode, WindowBoundary, WindowExclude, WindowExpr, sql_quoted_identifier,
};

/// What's bound of the window expressions of a SELECT.
#[derive(Debug, Default)]
pub(super) struct WindowBindState {
    pub(super) window_index: usize,
    pub(super) windows: Vec<BoundWindow>,
}

fn is_range_boundary(boundary: WindowBoundary) -> bool {
    matches!(
        boundary,
        WindowBoundary::ExprPrecedingRange | WindowBoundary::ExprFollowingRange
    )
}

fn is_range_type(ty: &LogicalType) -> bool {
    use PhysicalType::*;
    matches!(
        ty.physical_type(),
        Int8 | Int16 | Int32 | Int64 | Int128 | UInt8 | UInt16 | UInt32 | UInt64 | UInt128 | Float | Double | Interval
    )
}

fn window_function(name: &str) -> Option<WindowFunction> {
    use WindowFunction::*;
    Some(match name {
        "row_number" => RowNumber,
        "rank" => Rank,
        "dense_rank" | "rank_dense" => DenseRank,
        "percent_rank" => PercentRank,
        "cume_dist" => CumeDist,
        "ntile" => Ntile,
        "lag" => Lag,
        "lead" => Lead,
        "first_value" => FirstValue,
        "last_value" => LastValue,
        "nth_value" => NthValue,
        "fill" => Fill,
        _ => return None,
    })
}

impl Binder<'_> {
    /// `BaseSelectBinder::BindWindowExpression`.
    pub(super) fn bind_window(&mut self, window: &WindowExpr) -> Result<BoundExpr> {
        if self.windows.is_none() {
            return Err(binder_error(self.window_error));
        }
        if self.aggregates.as_ref().is_some_and(|state| state.inside_aggregate) {
            return Err(binder_error(
                "aggregate function calls cannot contain window function calls",
            ));
        }
        if self.inside_window {
            return Err(binder_error("window function calls cannot be nested"));
        }
        let name = window.name.name.to_lowercase();
        let function = window_function(&name);
        let aggregate_name = if name == "count" && window.args.is_empty() {
            "count_star"
        } else {
            name.as_str()
        };
        let set = match function {
            Some(_) => None,
            None => match BuiltinFunctions::get().aggregate(aggregate_name) {
                Some(set) => Some(set),
                None if BuiltinFunctions::get().scalar(aggregate_name).is_some()
                    || is_generic_function(aggregate_name) =>
                {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("{} is not an aggregate function", window.name.name),
                    ));
                }
                None => return Err(crate::catalog::missing_entry("Aggregate Function", &window.name.name)),
            },
        };
        let is_range = is_range_boundary(window.start) || is_range_boundary(window.end);
        if is_range && window.orders.len() != 1 {
            return Err(binder_error("RANGE frames must have only one ORDER BY expression"));
        }
        self.inside_window = true;
        let bound = self.bind_window_children(window, function, set, is_range);
        self.inside_window = false;
        let bound = bound?;
        let state = self.windows.as_mut().expect("checked above");
        state.windows.push(bound);
        let index = state.windows.len() - 1;
        let return_type = state.windows[index].return_type.clone();
        Ok(BoundExpr::column_ref(
            ColumnBinding::new(state.window_index, index),
            return_type,
            window.name.name.clone(),
        ))
    }

    fn bind_orders(&mut self, nodes: &[OrderByNode]) -> Result<Vec<(BoundExpr, OrderModifiers)>> {
        nodes
            .iter()
            .map(|node| {
                let expr = self.bind_expression(&node.expr)?;
                Ok((expr, self.options.resolve_order(node.order, node.nulls)))
            })
            .collect()
    }

    /// `BindRangeExpression`: the value of the order at a boundary of a RANGE frame.
    fn bind_range_boundary(
        &mut self,
        window: &WindowExpr,
        offset: &Expr,
        boundary: WindowBoundary,
        ascending: bool,
    ) -> Result<BoundExpr> {
        if self.bind_expression(offset)?.return_type.id() == LogicalTypeId::SqlNull {
            return Err(binder_error("Window RANGE expressions cannot be NULL"));
        }
        let forwards = (boundary == WindowBoundary::ExprFollowingRange) == ascending;
        let name = if forwards { "+" } else { "-" };
        let mut call = FunctionExpr::new(name, Vec::new());
        call.args = vec![
            FunctionArgument::positional(window.orders[0].expr.clone()),
            FunctionArgument::positional(offset.clone()),
        ];
        let bound = self.bind_expression(&call.into())?;
        if !is_range_type(&bound.return_type) {
            return Err(binder_error("Invalid type for Window RANGE expression"));
        }
        Ok(bound)
    }

    fn bind_window_children(
        &mut self,
        window: &WindowExpr,
        function: Option<WindowFunction>,
        set: Option<&crate::function::aggregates::AggregateFunctionSet>,
        is_range: bool,
    ) -> Result<BoundWindow> {
        let arguments = window
            .args
            .iter()
            .map(|argument| self.bind_expression(&argument.expr))
            .collect::<Result<Vec<_>>>()?;
        let partitions = window
            .partitions
            .iter()
            .map(|partition| self.bind_expression(partition))
            .collect::<Result<Vec<_>>>()?;
        let mut orders = self.bind_orders(&window.orders)?;
        if partitions.iter().any(BoundExpr::is_volatile) {
            return Err(binder_error("PARTITION BY window expressions cannot be volatile"));
        }
        if orders.iter().any(|(order, _)| order.is_volatile()) {
            return Err(binder_error("ORDER BY window expressions cannot be volatile"));
        }
        let filter = match &window.filter {
            Some(filter) => Some(self.bind_expression(filter)?.cast_to(&LogicalType::BOOLEAN, false)),
            None => None,
        };
        let arg_orders = self.bind_orders(&window.arg_orders)?;
        let mut start_expr = None;
        let mut end_expr = None;
        if is_range {
            let ascending = orders[0].1.order != OrderType::Descending;
            if let (Some(start), true) = (&window.start_expr, is_range_boundary(window.start)) {
                start_expr = Some(self.bind_range_boundary(window, start, window.start, ascending)?);
            }
            if let (Some(end), true) = (&window.end_expr, is_range_boundary(window.end)) {
                end_expr = Some(self.bind_range_boundary(window, end, window.end, ascending)?);
            }
            let mut order_type = orders[0].0.return_type.clone();
            for bound in start_expr.iter().chain(end_expr.iter()) {
                order_type = force_max_logical_type(&order_type, &bound.return_type);
            }
            orders[0].0 = orders[0].0.clone().cast_to(&order_type, false);
            start_expr = start_expr.map(|e| e.cast_to(&order_type, false));
            end_expr = end_expr.map(|e| e.cast_to(&order_type, false));
        }
        if !is_range_boundary(window.start)
            && let Some(start) = &window.start_expr
        {
            start_expr = Some(self.bind_expression(start)?.cast_to(&LogicalType::BIGINT, false));
        }
        if !is_range_boundary(window.end)
            && let Some(end) = &window.end_expr
        {
            end_expr = Some(self.bind_expression(end)?.cast_to(&LogicalType::BIGINT, false));
        }
        let (function, aggregate, return_type, mut children) = match function {
            Some(function) => {
                let arguments = in_parameter_order(function, window, arguments)?;
                let (return_type, children) =
                    window_arguments(function, &window.name.name, arguments, window, &orders, &arg_orders)?;
                (function, None, return_type, children)
            }
            None => {
                if window.has_ignore_nulls {
                    return Err(binder_error(
                        "RESPECT/IGNORE NULLS is not supported for windowed aggregates",
                    ));
                }
                let set = set.expect("an aggregate has its set");
                let bound = bind_aggregate_function(set, arguments, None, window.distinct)?;
                (
                    WindowFunction::Aggregate,
                    Some(bound.function),
                    bound.return_type,
                    bound.children,
                )
            }
        };
        if function == WindowFunction::Fill {
            // the second child is what FILL measures along
            let (along, _) = if let Some(order) = arg_orders.first() {
                order
            } else {
                &orders[0]
            };
            children.push(along.clone());
        }
        Ok(BoundWindow {
            function,
            aggregate,
            return_type,
            children,
            filter,
            distinct: window.distinct,
            ignore_nulls: window.ignore_nulls,
            partitions,
            orders,
            arg_orders,
            start: window.start,
            end: window.end,
            start_expr,
            end_expr,
            exclude: window.exclude,
        })
    }
}

/// The arguments in the order of the function's parameters, whatever names they came with.
fn in_parameter_order(
    function: WindowFunction,
    window: &WindowExpr,
    arguments: Vec<BoundExpr>,
) -> Result<Vec<BoundExpr>> {
    use WindowFunction::*;
    if window.args.iter().all(|a| a.name.is_empty()) {
        return Ok(arguments);
    }
    let parameters: &[&str] = match function {
        Lag | Lead => &["col", "offset", "default"],
        Ntile => &["num_buckets"],
        NthValue => &["expr", "n"],
        FirstValue | LastValue | Fill => &["expr"],
        _ => &[],
    };
    let mut slots: Vec<Option<BoundExpr>> = vec![None; parameters.len().max(arguments.len())];
    for (position, (argument, bound)) in window.args.iter().zip(arguments).enumerate() {
        let slot = if argument.name.is_empty() {
            position
        } else {
            parameters
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&argument.name))
                .ok_or_else(|| binder_error(format!("Unknown named argument {}", argument.name)))?
        };
        slots[slot] = Some(bound);
    }
    // DEFAULT without an offset
    if matches!(function, Lag | Lead) && slots[1].is_none() && slots.get(2).is_some_and(Option::is_some) {
        slots[1] = Some(BoundExpr::constant(Value::bigint(1)));
    }
    Ok(slots.into_iter().flatten().collect())
}

/// The checks of the window functions' signatures, and the casts of their arguments.
fn window_arguments(
    function: WindowFunction,
    name: &str,
    mut arguments: Vec<BoundExpr>,
    window: &WindowExpr,
    orders: &[(BoundExpr, OrderModifiers)],
    arg_orders: &[(BoundExpr, OrderModifiers)],
) -> Result<(LogicalType, Vec<BoundExpr>)> {
    use WindowFunction::*;
    let order_types: Vec<LogicalType> = orders.iter().map(|(e, _)| e.return_type.clone()).collect();
    let window_order_types: Vec<LogicalType> = arg_orders.iter().map(|(e, _)| e.return_type.clone()).collect();
    let no_match = |arguments: &[BoundExpr]| {
        let types: Vec<String> = arguments.iter().map(|a| a.return_type.to_string()).collect();
        binder_error(format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
             casts.",
            types.join(", ")
        ))
    };
    if window.has_ignore_nulls && !matches!(function, Lag | Lead | FirstValue | LastValue | NthValue) {
        return Err(binder_error(format!(
            "RESPECT/IGNORE NULLS is not supported for the window function {}",
            sql_quoted_identifier(name)
        )));
    }
    if window.distinct {
        return Err(binder_error(format!(
            "DISTINCT is not implemented for the window function {}",
            sql_quoted_identifier(name)
        )));
    }
    if window.exclude != WindowExclude::NoOther
        && !window.arg_orders.is_empty()
        && matches!(function, Lag | Lead | Ntile | Fill | RowNumber | Rank | CumeDist)
    {
        return Err(binder_error(format!(
            "EXCLUDE is not supported for the window function {}",
            sql_quoted_identifier(name)
        )));
    }
    let bigint = |argument: &mut BoundExpr| -> Result<()> {
        *argument = checked_cast(argument.clone(), &LogicalType::BIGINT)?;
        Ok(())
    };
    Ok(match function {
        RowNumber | Rank | DenseRank | PercentRank | CumeDist => {
            if !arguments.is_empty() {
                return Err(no_match(&arguments));
            }
            let ty = if matches!(function, PercentRank | CumeDist) {
                LogicalType::DOUBLE
            } else {
                LogicalType::BIGINT
            };
            (ty, arguments)
        }
        Ntile => {
            if arguments.len() != 1 {
                return Err(no_match(&arguments));
            }
            bigint(&mut arguments[0])?;
            (LogicalType::BIGINT, arguments)
        }
        Lag | Lead => {
            if arguments.is_empty() || arguments.len() > 3 {
                return Err(no_match(&arguments));
            }
            let ty = arguments[0].return_type.clone();
            if arguments.len() > 1 {
                bigint(&mut arguments[1])?;
            }
            if arguments.len() > 2 {
                arguments[2] = arguments[2].clone().cast_to(&ty, false);
            }
            (ty, arguments)
        }
        FirstValue | LastValue => {
            if arguments.len() != 1 {
                return Err(no_match(&arguments));
            }
            (arguments[0].return_type.clone(), arguments)
        }
        NthValue => {
            if arguments.len() != 2 {
                return Err(no_match(&arguments));
            }
            bigint(&mut arguments[1])?;
            (arguments[0].return_type.clone(), arguments)
        }
        Fill => {
            if arguments.len() != 1 {
                return Err(no_match(&arguments));
            }
            if window.arg_orders.len() > 1 || (window.arg_orders.is_empty() && orders.len() != 1) {
                return Err(binder_error("FILL functions must have only one ORDER BY expression"));
            }
            let is_number = |ty: &LogicalType| {
                !matches!(
                    ty.physical_type(),
                    PhysicalType::Varchar
                        | PhysicalType::List
                        | PhysicalType::Struct
                        | PhysicalType::Array
                        | PhysicalType::Bool
                )
            };
            if !is_number(&arguments[0].return_type) {
                return Err(binder_error("FILL argument must support subtraction"));
            }
            let order = window_order_types.first().or(order_types.first());
            if order.is_some_and(|ty| !is_number(ty)) {
                return Err(binder_error("FILL ordering must support subtraction"));
            }
            (arguments[0].return_type.clone(), arguments)
        }
        Aggregate => unreachable!("aggregates have no window signature"),
    })
}
