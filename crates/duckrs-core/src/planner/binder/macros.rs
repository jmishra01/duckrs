// Ported from duckdb/src/function/macro_function.cpp,
// duckdb/src/planner/binder/expression/bind_macro_expression.cpp,
// duckdb/src/planner/binder/query_node/bind_table_macro_node.cpp and the macro parts of
// duckdb/src/planner/binder/statement/bind_create.cpp and
// duckdb/src/planner/binder/tableref/bind_table_function.cpp @ rust-port-baseline
//
//! Macros. A call expands into the macro's parsed body with the arguments put in place of the
//! parameters, and that binds like anything the query could have spelled out itself.

use super::expression::checked_cast;
use super::{Binder, binder_error};
use crate::catalog::MacroCatalogEntry;
use crate::function::binder::expression_return_type;
use crate::function::cast_rules::implicit_cast;
use crate::planner::expression::BoundExpr;
use crate::planner::logical::LogicalOperator;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::{ErrorKind, Result};
use duckrs_parser::ast::sql_quoted_identifier;
use duckrs_parser::ast::{
    CastType, Expr, ExprKind, FunctionExpr, Literal, LiteralKind, MacroBody, MacroFunction, QualifiedName, QueryNode,
    SelectStatement, TableRef, sql_identifier,
};
use duckrs_parser::transformer::{visit_all_query_node_exprs_mut as visit_query_node_exprs_mut, visit_expr_mut};
use std::sync::Arc;

/// How deep macros may nest. DuckDB counts expression depth against `max_expression_depth`,
/// which also ends a macro calling itself; this limit keeps the recursion within the stack.
const MAX_MACRO_DEPTH: usize = 32;

/// Marks a parameter while arguments are put in, so that a column of an argument that is named
/// like a parameter stays what it is.
const PARAMETER_MARK: &str = "\0macro parameter";

/// Marks a parameter of a macro being created, which the binder binds (`Binder::macro_binding`).
pub(super) const CREATE_PARAMETER_MARK: &str = "\0created macro parameter";

/// Turns the references to `parameters` into marked references to their position. A call on a
/// parameter, as in `p.lower()`, is a call with it (`ColumnQualifier::QualifyFunction`).
fn mark_parameters(expr: &mut Expr, parameters: &[String]) {
    if let ExprKind::Function(function) = &mut expr.kind
        && let [name] = function.name.qualification.as_slice()
        && let Some(i) = parameters.iter().position(|p| p.eq_ignore_ascii_case(name))
        && super::expression::is_system_function(&function.name.name)
    {
        function.name.qualification.clear();
        let marked = ExprKind::ColumnRef(vec![PARAMETER_MARK.to_string(), i.to_string()]);
        function.args.insert(
            0,
            duckrs_parser::ast::FunctionArgument {
                name: String::new(),
                expr: marked.into(),
            },
        );
        return;
    }
    if let ExprKind::ColumnRef(names) = &expr.kind
        && let Some(first) = names.first()
        && let Some(i) = parameters.iter().position(|p| p.eq_ignore_ascii_case(first))
    {
        // `p.a.b` is a field of the parameter (`ColumnQualifier::QualifyColumnNameWithManyDots`)
        let fields = names[1..].to_vec();
        let mut marked = Expr::from(ExprKind::ColumnRef(vec![PARAMETER_MARK.to_string(), i.to_string()]));
        for field in fields {
            marked = Expr::operator(
                duckrs_parser::ast::OperatorType::StructExtract,
                vec![marked, Expr::constant(Literal::string(&field))],
            );
        }
        marked.alias = std::mem::take(&mut expr.alias);
        *expr = marked;
    }
}

/// Hides the parameters of the lambdas in `expr` that are named like a macro parameter, which
/// they shadow in the lambda (`ColumnQualifier` binds lambda parameters first).
const LAMBDA_MARK: char = '\u{3}';

fn protect_lambda_parameters(expr: &mut Expr, parameters: &[String]) {
    // `ReplaceMacroParametersInLambda`: only the lambda of a lambda function, as `->` is also JSON
    let ExprKind::Function(function) = &mut expr.kind else {
        return;
    };
    if !super::lambda::is_lambda_function(&function.name.name) {
        return;
    }
    for argument in &mut function.args {
        protect_lambda(&mut argument.expr, parameters);
    }
}

fn protect_lambda(expr: &mut Expr, parameters: &[String]) {
    let ExprKind::Lambda { lhs, expr: body, .. } = &mut expr.kind else {
        return;
    };
    let mut shadowed = Vec::new();
    visit_expr_mut(lhs, &mut |e| {
        if let ExprKind::ColumnRef(names) = &mut e.kind
            && let [name] = names.as_mut_slice()
            && parameters.iter().any(|p| p.eq_ignore_ascii_case(name))
        {
            shadowed.push(name.clone());
            name.insert(0, LAMBDA_MARK);
        }
    });
    visit_expr_mut(body, &mut |e| {
        if let ExprKind::ColumnRef(names) = &mut e.kind
            && let Some(first) = names.first_mut()
            && shadowed.iter().any(|s| s.eq_ignore_ascii_case(first))
        {
            first.insert(0, LAMBDA_MARK);
        }
    });
}

fn unprotect_lambda_parameters(expr: &mut Expr) {
    if let ExprKind::ColumnRef(names) = &mut expr.kind
        && let Some(first) = names.first_mut()
        && first.starts_with(LAMBDA_MARK)
    {
        first.remove(0);
    }
}

/// [`mark_parameters`] over `expr` and the subqueries in it, but not over lambda parameters.
fn mark_all(expr: &mut Expr, parameters: &[String]) {
    visit_expr_mut(expr, &mut |e| {
        into_subqueries(e, &mut |e| protect_lambda_parameters(e, parameters))
    });
    visit_expr_mut(expr, &mut |e| {
        into_subqueries(e, &mut |e| mark_parameters(e, parameters))
    });
    visit_expr_mut(expr, &mut |e| into_subqueries(e, &mut unprotect_lambda_parameters));
}

/// Puts the arguments in place of the marked references (`DummyBinding::ParamToArg`).
fn replace_marked(expr: &mut Expr, arguments: &[Expr]) {
    if let ExprKind::ColumnRef(names) = &expr.kind
        && let [mark, position] = names.as_slice()
        && mark == PARAMETER_MARK
    {
        let alias = std::mem::take(&mut expr.alias);
        *expr = arguments[position.parse::<usize>().expect("a marked position")].clone();
        expr.alias = alias;
    }
}

/// `op` on `expr`, and on the expressions of the subqueries in it, as
/// `ReplaceMacroParameters` goes into them.
fn into_subqueries(expr: &mut Expr, op: &mut dyn FnMut(&mut Expr)) {
    op(expr);
    if let ExprKind::Subquery { subquery, .. } = &mut expr.kind {
        visit_query_node_exprs_mut(&mut subquery.node, &mut |child| into_subqueries(child, op));
    }
}

/// `ExpressionBinder::ReplaceMacroParameters` over a scalar macro's body.
fn replace_in_expr(body: &mut Expr, parameters: &[String], arguments: &[Expr]) {
    mark_all(body, parameters);
    visit_expr_mut(body, &mut |e| into_subqueries(e, &mut |e| replace_marked(e, arguments)));
}

/// `ExpressionBinder::ReplaceMacroParameters` over a table macro's query.
fn replace_in_query(body: &mut QueryNode, parameters: &[String], arguments: &[Expr]) {
    visit_query_node_exprs_mut(body, &mut |e| mark_all(e, parameters));
    visit_query_node_exprs_mut(body, &mut |e| into_subqueries(e, &mut |e| replace_marked(e, arguments)));
}

fn cast_to(expr: Expr, target: &CastType) -> Expr {
    ExprKind::Cast {
        child: Box::new(expr),
        target: target.clone(),
        try_cast: false,
    }
    .into()
}

fn parameter_names(function: &MacroFunction) -> Vec<String> {
    function.parameters.iter().map(|p| p.name.clone()).collect()
}

/// `FormatMacroFunction`: an overload for the "candidates" of an error.
fn format_macro(function: &MacroFunction, name: &str) -> String {
    let parameters: Vec<String> = function
        .parameters
        .iter()
        .map(|parameter| {
            let mut text = parameter.name.clone();
            if let Some(ty) = &parameter.parameter_type {
                text += &format!(" {ty}");
            }
            if let Some(default) = &parameter.default {
                text += &format!(" := {default}");
            }
            text
        })
        .collect();
    format!("{}({})", sql_identifier(name), parameters.join(", "))
}

/// Whether a parsed expression has a prepared statement parameter in it.
pub(super) fn has_parameter(expr: &Expr) -> bool {
    let mut found = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        found |= matches!(e.kind, ExprKind::Parameter(_));
    });
    found
}

impl Binder<'_> {
    /// The user's macro this name refers to, of the kind wanted if there are both. Macros come
    /// before the built-in functions on the search path, and a qualification that does not name
    /// a schema of theirs means a built-in.
    pub(super) fn find_macro(&self, name: &QualifiedName, table_macro: bool) -> Result<Option<Arc<MacroCatalogEntry>>> {
        let Ok(schema) = self.schema_of(name) else {
            return Ok(None);
        };
        let schema = schema.as_deref();
        if let Some(schema) = schema
            && self.catalog.get_schema(schema).is_none()
        {
            return Ok(None);
        }
        // `system.main.sum` is `main` of another catalog
        if schema.is_some() && name.qualification.len() == 2 && name.qualification[0].eq_ignore_ascii_case("system") {
            return Ok(None);
        }
        let search_first = self.search_schema.as_deref();
        let wanted = self.catalog.get_macro(schema, search_first, &name.name, table_macro)?;
        match wanted {
            Some(entry) => Ok(Some(entry)),
            None => self.catalog.get_macro(schema, search_first, &name.name, !table_macro),
        }
    }

    /// `MacroFunction::BindMacroFunction`: picks the overload, and returns it with an argument
    /// for each of its parameters, in order.
    fn bind_macro_function<'m>(
        &mut self,
        entry: &'m MacroCatalogEntry,
        call: &FunctionExpr,
    ) -> Result<(&'m MacroFunction, Vec<Expr>)> {
        let name = &entry.name;
        let quoted = sql_quoted_identifier(name);
        let requires_bind = entry
            .macros
            .iter()
            .any(|m| m.parameters.iter().any(|p| p.parameter_type.is_some()));
        // separate the positional and the named arguments, with their types if types matter
        let unknown = LogicalType::new(LogicalTypeId::Unknown);
        let mut positional: Vec<(&Expr, LogicalType)> = Vec::new();
        let mut named: Vec<(&str, &Expr, LogicalType)> = Vec::new();
        for argument in &call.args {
            let ty = if requires_bind {
                self.bind_expression(&argument.expr)
                    .map_or(unknown.clone(), |bound| expression_return_type(&bound))
            } else {
                unknown.clone()
            };
            if !argument.name.is_empty() {
                if named.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&argument.name)) {
                    return Err(binder_error(format!(
                        "Macro {quoted}() has named argument repeated {}",
                        sql_quoted_identifier(&argument.name)
                    )));
                }
                named.push((&argument.name, &argument.expr, ty));
            } else if !named.is_empty() {
                return Err(binder_error(format!(
                    "Macro {quoted}() has positional argument following named argument"
                )));
            } else {
                positional.push((&argument.expr, ty));
            }
        }
        // the cheapest overload that takes the arguments
        let mut lowest_cost = i64::MAX;
        let mut matches: Vec<usize> = Vec::new();
        for (index, function) in entry.macros.iter().enumerate() {
            if let Some(cost) = self.macro_cost(function, &positional, &named) {
                if cost < lowest_cost {
                    lowest_cost = cost;
                    matches.clear();
                }
                if cost == lowest_cost {
                    matches.push(index);
                }
            }
        }
        let call_name = sql_identifier(name);
        let candidates = |indexes: &mut dyn Iterator<Item = usize>| -> String {
            indexes
                .map(|i| format!("\n\t{}", format_macro(&entry.macros[i], name)))
                .collect()
        };
        let function = match matches.as_slice() {
            [index] => &entry.macros[*index],
            [] => {
                return Err(binder_error(format!(
                    "Macro {call_name}() does not support the supplied arguments. You might need to add explicit type \
                     casts.\nCandidate macros:{}",
                    candidates(&mut (0..entry.macros.len()))
                )));
            }
            _ => {
                return Err(binder_error(format!(
                    "Macro {call_name}() has multiple overloads that match the supplied arguments.\nIn order to select \
                     one, please supply all arguments by name, and/or add explicit type casts.\nCandidate macros:{}",
                    candidates(&mut matches.iter().copied())
                )));
            }
        };
        // an argument for each parameter, cast to the parameter's type
        let mut arguments = Vec::with_capacity(function.parameters.len());
        for (i, parameter) in function.parameters.iter().enumerate() {
            let argument = match positional.get(i) {
                Some((argument, _)) => (*argument).clone(),
                None => named
                    .iter()
                    .find(|(n, ..)| n.eq_ignore_ascii_case(&parameter.name))
                    .map(|(_, argument, _)| (*argument).clone())
                    .or_else(|| parameter.default.clone())
                    .expect("the overload has a value for each parameter"),
            };
            arguments.push(match &parameter.parameter_type {
                Some(ty) => cast_to(argument, ty),
                None => argument,
            });
        }
        Ok((function, arguments))
    }

    /// What it costs to call this overload with these arguments, if it can take them.
    fn macro_cost(
        &self,
        function: &MacroFunction,
        positional: &[(&Expr, LogicalType)],
        named: &[(&str, &Expr, LogicalType)],
    ) -> Option<i64> {
        let parameters = &function.parameters;
        if positional.len() + named.len() > parameters.len() {
            return None;
        }
        let known = |name: &str| parameters.iter().any(|p| p.name.eq_ignore_ascii_case(name));
        if !named.iter().all(|(name, ..)| known(name)) {
            return None;
        }
        let mut total = 0;
        let cost = |ty: &Option<CastType>, argument: &LogicalType| -> Option<i64> {
            match ty {
                None => Some(1_000_000),
                Some(_) if argument.id() == LogicalTypeId::Unknown => Some(0),
                Some(ty) => {
                    let target = self.bind_type(ty).ok()?;
                    let cost = implicit_cast(argument, &target);
                    (cost >= 0).then_some(cost)
                }
            }
        };
        for (i, parameter) in parameters.iter().enumerate() {
            if let Some((_, ty)) = positional.get(i) {
                if named.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&parameter.name)) {
                    return None;
                }
                total += cost(&parameter.parameter_type, ty)?;
            } else if let Some((.., ty)) = named.iter().find(|(n, ..)| n.eq_ignore_ascii_case(&parameter.name)) {
                total += cost(&parameter.parameter_type, ty)?;
            } else if parameter.default.is_none() {
                return None;
            }
        }
        Some(total)
    }

    /// `ExpressionBinder::BindMacro`.
    pub(super) fn bind_macro(&mut self, call: &FunctionExpr, entry: &MacroCatalogEntry) -> Result<BoundExpr> {
        if entry.table_macro {
            return Err(binder_error(format!(
                "Function {} is a table function but it was used as a scalar function. This function has to be called \
                 in a FROM clause (similar to a table).",
                sql_quoted_identifier(&entry.name)
            )));
        }
        if call.filter.is_some() || !call.order_by.is_empty() || call.distinct {
            return Err(crate::unported("modifiers on macros"));
        }
        if self.macro_depth >= MAX_MACRO_DEPTH {
            return Err(binder_error(
                "Max expression depth limit of 1000 exceeded. Use \"SET max_expression_depth TO x\" to increase the \
                 maximum expression depth.",
            ));
        }
        let (function, arguments) = self.bind_macro_function(entry, call)?;
        let MacroBody::Scalar(body) = &function.body else {
            unreachable!("a scalar macro has a scalar body");
        };
        let mut body = (**body).clone();
        replace_in_expr(&mut body, &parameter_names(function), &arguments);
        self.macro_depth += 1;
        let bound = self.bind_expression(&body);
        self.macro_depth -= 1;
        let bound = bound?;
        match &function.return_type {
            Some(ty) => checked_cast(bound, &self.bind_type(ty)?),
            None => Ok(bound),
        }
    }

    /// `Binder::BindTableMacro` + the macro branch of `Binder::Bind(TableFunctionRef)`.
    pub(super) fn bind_table_macro(
        &mut self,
        table_ref: &TableRef,
        call: &FunctionExpr,
        entry: Arc<MacroCatalogEntry>,
    ) -> Result<LogicalOperator> {
        if !entry.table_macro {
            return Err(crate::catalog::missing_entry("Table Function", &entry.name));
        }
        if self.bound_macros.iter().any(|bound| Arc::ptr_eq(bound, &entry)) {
            return Err(binder_error(format!(
                "infinite recursion detected: attempting to recursively expand table macro \"{}\"",
                entry.name
            )));
        }
        let (function, arguments) = self.bind_macro_function(&entry, call)?;
        let MacroBody::Table(body) = &function.body else {
            unreachable!("a table macro has a query");
        };
        let mut body = (**body).clone();
        replace_in_query(&mut body, &parameter_names(function), &arguments);
        let mut binder = self.child_binder();
        binder.left_of_join = std::mem::take(&mut self.left_of_join);
        binder.bound_macros.push(entry.clone());
        let bound = binder.bind_select_statement(&SelectStatement { node: body });
        self.left_of_join = std::mem::take(&mut binder.left_of_join);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let alias = if table_ref.alias.is_empty() {
            format!("unnamed_query{}", bound.plan.column_bindings()[0].table_index)
        } else {
            table_ref.alias.clone()
        };
        self.add_query_binding(alias, bound, &table_ref.column_name_alias)
    }

    /// `Binder::BindCreateFunctionInfo`: checks the overloads of a macro being created. Their
    /// bodies bind with NULLs for the parameters, whose types are not known yet.
    pub fn bind_create_macro(
        &mut self,
        name: &str,
        macros: &[MacroFunction],
        table_macro: bool,
        schema: &str,
    ) -> Result<()> {
        let mut signatures: Vec<Vec<Option<LogicalType>>> = Vec::with_capacity(macros.len());
        for function in macros {
            let mut signature = Vec::with_capacity(function.parameters.len());
            for parameter in &function.parameters {
                let ty = parameter
                    .parameter_type
                    .as_ref()
                    .map(|t| self.bind_type(t))
                    .transpose()?;
                if let Some(default) = &parameter.default {
                    self.check_macro_default(&parameter.name, default, ty.as_ref())?;
                }
                signature.push(ty);
            }
            if let Some(ty) = &function.return_type {
                self.bind_type(ty)?;
            }
            if signatures.contains(&signature) {
                return Err(binder_error(format!(
                    "Ambiguity in macro overloads - macro {name}() has multiple definitions with the same parameters"
                )));
            }
            signatures.push(signature);
            let nulls: Vec<Expr> = function
                .parameters
                .iter()
                .map(|parameter| {
                    let null = Expr::from(ExprKind::Constant(Literal {
                        kind: LiteralKind::Null,
                        text: String::new(),
                    }));
                    match &parameter.parameter_type {
                        Some(ty) => cast_to(null, ty),
                        None => null,
                    }
                })
                .collect();
            let mut binder = Binder::with_options(self.catalog, self.options.clone());
            binder.search_schema = Some(schema.to_string());
            let parameters = parameter_names(function);
            match &function.body {
                MacroBody::Scalar(body) => {
                    if has_parameter(body) {
                        return Err(binder_error("Parameter expressions within macro's are not supported!"));
                    }
                    let mut body = (**body).clone();
                    mark_all(&mut body, &parameters);
                    visit_expr_mut(&mut body, &mut |e| {
                        into_subqueries(e, &mut |e| {
                            if let ExprKind::ColumnRef(names) = &mut e.kind
                                && names.len() == 2
                                && names[0] == PARAMETER_MARK
                            {
                                names[0] = CREATE_PARAMETER_MARK.to_string();
                            }
                        })
                    });
                    binder.macro_arguments = parameters.iter().cloned().zip(nulls.iter().cloned()).collect();
                    // a macro's body may aggregate, and whether that fits shows where it is used
                    binder.aggregate_error = MACRO_AGGREGATE;
                    match binder.bind_expression(&body) {
                        // `BindCreateFunctionInfo`: what can't be bound yet is bound when it's called
                        Err(error)
                            if error.message == MACRO_AGGREGATE || error.kind == ErrorKind::ParameterNotResolved => {}
                        result => {
                            result?;
                        }
                    }
                }
                MacroBody::Table(body) => {
                    debug_assert!(table_macro);
                    let mut body = (**body).clone();
                    let mut parameter_found = false;
                    visit_query_node_exprs_mut(&mut body, &mut |e| {
                        parameter_found |= matches!(e.kind, ExprKind::Parameter(_));
                    });
                    if parameter_found {
                        return Err(binder_error("Parameter expressions within macro's are not supported!"));
                    }
                    replace_in_query(&mut body, &parameters, &nulls);
                    binder.bind_select_statement(&SelectStatement { node: body })?;
                }
            }
        }
        Ok(())
    }

    /// A default value has to be a constant the parameter's type accepts.
    fn check_macro_default(&mut self, parameter: &str, default: &Expr, ty: Option<&LogicalType>) -> Result<()> {
        // `ConstantBinder` for the clause "Default value for parameter p"
        let clause = format!("Default value for parameter {}", sql_quoted_identifier(parameter));
        let mut unsupported = None;
        visit_expr_mut(&mut default.clone(), &mut |e| {
            let message = match &e.kind {
                ExprKind::ColumnRef(_) => "cannot contain column names",
                ExprKind::Subquery { .. } => "cannot contain subqueries",
                _ => return,
            };
            unsupported.get_or_insert_with(|| format!("{clause} {message}"));
        });
        if let Some(message) = unsupported {
            return Err(binder_error(message));
        }
        let mut binder = Binder::new(self.catalog);
        let bound = binder.bind_expression(default)?;
        if !bound.is_foldable() {
            return Err(binder_error(format!(
                "Default value '{default}' for parameter {} is not a constant expression.",
                sql_quoted_identifier(parameter)
            )));
        }
        if let Some(ty) = ty
            && implicit_cast(&expression_return_type(&bound), ty) < 0
        {
            return Err(binder_error(format!(
                "Default value '{default}' for parameter {} cannot be implicitly cast to '{ty}'. Please add an \
                 explicit type cast.",
                sql_quoted_identifier(parameter)
            )));
        }
        Ok(())
    }
}

/// Stands in for the error of an aggregate while a macro's body is checked.
const MACRO_AGGREGATE: &str = "\0macro aggregate";
