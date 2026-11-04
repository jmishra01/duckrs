// Ported from duckdb/src/planner/binder/expression/bind_star_expression.cpp @ rust-port-baseline
//
//! `*`, `COLUMNS(...)` and `*COLUMNS(...)`: what they stand for in a select list.

use super::expression::without_aliases;
use super::{Binder, binder_error};
use crate::execution::expression_executor::evaluate_scalar;
use duckrs_common::Result;
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::LogicalTypeId;
use duckrs_common::value::{Value, ValueData};
use duckrs_parser::ast::{Expr, ExprKind, FunctionExpr, Literal, LiteralKind, OperatorType, StarExpr};
use duckrs_parser::transformer::visit_expr_mut;
use regex::Regex;

fn is_plain_star(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Star(star) if !star.columns && star.expr.is_none())
}

/// `TryTransformStarLike`: `* LIKE 'x'` picks columns by name.
fn transform_star_like(root: &mut Expr) -> Result<()> {
    let inverse = matches!(
        &root.kind,
        ExprKind::Operator {
            op: OperatorType::Not,
            ..
        }
    );
    let alias = root.alias.clone();
    let expr = if inverse {
        let ExprKind::Operator { children, .. } = &root.kind else {
            unreachable!("checked above");
        };
        &children[0]
    } else {
        &*root
    };
    let ExprKind::Function(function) = &expr.kind else {
        return Ok(());
    };
    if !(2..=3).contains(&function.args.len()) || !is_plain_star(&function.args[0].expr) {
        return Ok(());
    }
    let ExprKind::Star(star) = &function.args[0].expr.kind else {
        unreachable!("checked above");
    };
    const SUPPORTED: [&str; 12] = [
        "~~",
        "!~~",
        "~~~",
        "!~~~",
        "~~*",
        "!~~*",
        "regexp_full_match",
        "regexp_matches",
        "not_like_escape",
        "ilike_escape",
        "not_ilike_escape",
        "like_escape",
    ];
    let name = function.name.name.as_str();
    if !SUPPORTED.contains(&name) {
        return Err(binder_error(format!(
            "Function {name} cannot be applied to a star expression"
        )));
    }
    let right = &function.args[1].expr;
    if !matches!(right.kind, ExprKind::Constant(_)) {
        return Err(binder_error("Pattern applied to a star expression must be a constant"));
    }
    if !star.rename_list.is_empty() {
        return Err(binder_error(
            "Rename list cannot be combined with a filtering operation",
        ));
    }
    if !star.replace_list.is_empty() {
        return Err(binder_error(
            "Replace list cannot be combined with a filtering operation",
        ));
    }
    let child = if !inverse && name == "regexp_full_match" && function.args.len() == 2 && star.exclude_list.is_empty() {
        let ExprKind::Constant(pattern) = &right.kind else {
            unreachable!("checked above");
        };
        Expr::constant(Literal::string(format!("^(?:{})$", pattern.text)))
    } else {
        let mut body = function.clone();
        body.args[0].expr = Expr::column(vec!["__lambda_col".to_string()]);
        let mut body: Expr = ExprKind::Function(body).into();
        if inverse {
            body = Expr::not_op(body);
        }
        let lambda = Expr::lambda(&["__lambda_col".to_string()], body);
        FunctionExpr::new("list_filter", vec![function.args[0].expr.clone(), lambda]).into()
    };
    let columns = StarExpr {
        relation_name: star.relation_name.clone(),
        columns: true,
        expr: Some(Box::new(child)),
        ..StarExpr::default()
    };
    *root = ExprKind::Star(columns).into();
    root.alias = alias;
    Ok(())
}

/// `GetResolvedColumnExpression`: the column an expanded expression is, if it is one.
fn resolved_column(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::ColumnRef(names) => names.last().cloned(),
        ExprKind::Operator {
            op: OperatorType::Coalesce,
            children,
        } => resolved_column(&children[0]),
        _ => None,
    }
}

/// `GetColumnsStringValue`.
fn columns_string(expr: &Expr) -> String {
    resolved_column(expr)
        .filter(|_| matches!(expr.kind, ExprKind::ColumnRef(_)))
        .or_else(|| (!expr.alias.is_empty()).then(|| expr.alias.clone()))
        .unwrap_or_else(|| expr.to_string())
}

/// `Binder::ReplaceColumnsAlias`.
fn replace_columns_alias(alias: &str, column: &str, regex: Option<&Regex>) -> Result<String> {
    let mut result = String::with_capacity(alias.len());
    let mut chars = alias.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        let Some(code) = chars.next() else {
            return Err(binder_error(format!(
                "Unterminated backslash in COLUMNS(*) \"{alias}\" alias. Backslashes must either be escaped or \
                 followed by a number"
            )));
        };
        match code {
            '\\' => result.push('\\'),
            '0' => result.push_str(column),
            '1'..='9' => {
                let Some(regex) = regex else {
                    return Err(binder_error(
                        "Only the backslash escape code \\0 can be used when no regex is supplied to COLUMNS(*)",
                    ));
                };
                let group = code.to_digit(10).expect("a digit") as usize;
                if let Some(found) = regex.captures(column).and_then(|c| c.get(group)) {
                    result.push_str(found.as_str());
                }
            }
            _ => {
                return Err(binder_error(format!(
                    "Invalid backslash code in COLUMNS(*) \"{alias}\" alias. Backslashes must either be escaped or \
                     followed by a number"
                )));
            }
        }
    }
    Ok(result)
}

fn is_qualified_count_star(expr: &Expr) -> bool {
    let ExprKind::Function(function) = &expr.kind else {
        return false;
    };
    if !function.name.name.eq_ignore_ascii_case("count") || function.distinct || function.args.len() != 1 {
        return false;
    }
    matches!(&function.args[0].expr.kind, ExprKind::Star(star)
        if !star.columns
            && star.expr.is_none()
            && !star.relation_name.is_empty()
            && star.exclude_list.is_empty()
            && star.replace_list.is_empty()
            && star.rename_list.is_empty())
}

impl Binder<'_> {
    /// `Binder::TryRewriteQualifiedCountStar`: `count(t.*)` skips the rows an outer join made up.
    fn rewrite_qualified_count_star(&self, expr: &mut Expr) {
        let name = expr.to_string();
        let ExprKind::Function(function) = &mut expr.kind else {
            return;
        };
        let ExprKind::Star(star) = &function.args[0].expr.kind else {
            return;
        };
        let binding = self
            .bind_context
            .bindings
            .iter()
            .find(|b| b.alias.eq_ignore_ascii_case(&star.relation_name));
        match binding {
            Some(binding) if binding.row_id.is_some() && binding.column_index("rowid").is_none() => {
                function.args[0].expr = Expr::column(vec![binding.alias.clone(), "rowid".to_string()]);
            }
            _ => {
                function.args.clear();
                function.name.name = "count_star".to_string();
            }
        }
        if expr.alias.is_empty() {
            expr.alias = name;
        }
    }

    /// A `*` inside of `COLUMNS` is the list of the names it stands for.
    fn columns_argument(&self, argument: &mut Expr) -> Result<()> {
        let mut error = None;
        visit_expr_mut(argument, &mut |e| {
            let ExprKind::Star(star) = &e.kind else {
                return;
            };
            if star.columns {
                error = Some(binder_error(
                    "COLUMNS expression is not allowed inside another COLUMNS expression",
                ));
                return;
            }
            if !star.replace_list.is_empty() {
                error = Some(binder_error(
                    "STAR expression with REPLACE list is only allowed as the root element of COLUMNS",
                ));
                return;
            }
            if !star.rename_list.is_empty() {
                error = Some(binder_error(
                    "STAR expression with RENAME list is only allowed as the root element of COLUMNS",
                ));
                return;
            }
            match self.generate_all_columns(star) {
                Ok(columns) => {
                    let names = columns
                        .iter()
                        .map(|c| Expr::constant(Literal::string(columns_string(c))))
                        .collect();
                    *e = FunctionExpr::new("list_value", names).into();
                }
                Err(e) => error = Some(e),
            }
        });
        error.map_or(Ok(()), Err)
    }

    /// `FindStarExpression` below the root: rewrites `count(t.*)` and returns the `COLUMNS` of `expr`.
    fn find_columns(&self, expr: &mut Expr) -> Result<(Option<StarExpr>, bool)> {
        let mut found: Option<StarExpr> = None;
        let mut unpacked = false;
        let mut error = None;
        visit_expr_mut(expr, &mut |e| {
            if error.is_some() {
                return;
            }
            if is_qualified_count_star(e) {
                self.rewrite_qualified_count_star(e);
                return;
            }
            if matches!(
                &e.kind,
                ExprKind::Operator {
                    op: OperatorType::Unpack,
                    ..
                }
            ) {
                unpacked = true;
                return;
            }
            let ExprKind::Star(star) = &mut e.kind else {
                return;
            };
            if !star.columns {
                error = Some(binder_error(
                    "STAR expression is only allowed as the root element of an expression. Use COLUMNS(*) instead.",
                ));
                return;
            }
            if let Some(argument) = &mut star.expr
                && let Err(e) = self.columns_argument(argument)
            {
                error = Some(e);
                return;
            }
            match &found {
                Some(existing) if existing != star => {
                    error = Some(binder_error(
                        "Multiple different STAR/COLUMNS in the same expression are not supported",
                    ));
                }
                _ => found = Some(star.clone()),
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        if unpacked && found.is_none() {
            let mut inner_found = None;
            let mut plain_star = None;
            visit_expr_mut(expr, &mut |e| {
                if let ExprKind::Operator {
                    op: OperatorType::Unpack,
                    children,
                } = &mut e.kind
                {
                    if let ExprKind::Star(star) = &mut children[0].kind {
                        if star.columns {
                            inner_found = Some(star.clone());
                        } else {
                            plain_star = Some(star.clone());
                        }
                    } else {
                        error = Some(binder_error(
                            "UNPACK can only be used in combination with a STAR (*) expression or COLUMNS expression",
                        ));
                    }
                }
            });
            if let Some(error) = error {
                return Err(error);
            }
            found = inner_found.or(plain_star);
        }
        Ok((found, unpacked))
    }

    /// The columns a `COLUMNS(expr)` selects of `columns`, and the regex if `expr` is one.
    fn filter_columns(&self, star: &StarExpr, columns: Vec<Expr>) -> Result<(Vec<Expr>, Option<Regex>)> {
        let Some(argument) = &star.expr else {
            return Ok((columns, None));
        };
        let mut binder = Binder::new(self.catalog);
        binder.parameters = self.parameters.clone();
        let bound = binder.bind_expression(argument)?;
        if !bound.is_foldable() {
            return Err(binder_error("Unsupported expression in COLUMNS"));
        }
        let value = evaluate_scalar(&bound)?;
        match value.logical_type().id() {
            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral => {
                if value.is_null() {
                    return Err(binder_error("COLUMNS does not support NULL as regex argument"));
                }
                let pattern = value.try_to_string()?;
                let regex = Regex::new(&pattern)
                    .map_err(|e| binder_error(format!("Failed to compile regex \"{pattern}\": {e}")))?;
                let names: Vec<String> = columns.iter().filter_map(resolved_column).collect();
                let selected: Vec<Expr> = columns
                    .into_iter()
                    .filter(|c| resolved_column(c).is_some_and(|name| regex.is_match(&name)))
                    .collect();
                if selected.is_empty() {
                    let scores = names
                        .into_iter()
                        .map(|name| {
                            let score = similarity_rating(&name, &pattern);
                            (name, score)
                        })
                        .collect();
                    return Err(binder_error(format!(
                        "No matching columns found that match regex \"{pattern}\"{}",
                        candidates_message(&top_n_strings(scores), "Did you mean")
                    )));
                }
                Ok((selected, Some(regex)))
            }
            LogicalTypeId::List | LogicalTypeId::SqlNull => {
                let items = match value.data() {
                    ValueData::List(items) => items.as_slice(),
                    _ => &[],
                };
                if value.is_null() || items.is_empty() {
                    return Err(binder_error(format!(
                        "Star expression \"{}\" resulted in an empty set of columns",
                        Expr::from(ExprKind::Star(star.clone()))
                    )));
                }
                let mut wanted = Vec::with_capacity(items.len());
                for item in items {
                    if item.is_null() {
                        return Err(binder_error(
                            "Columns expression does not support NULL input parameters",
                        ));
                    }
                    wanted.push((item.try_to_string()?, false));
                }
                let mut selected = Vec::new();
                for column in columns {
                    let name = columns_string(&column);
                    let matches: Vec<_> = wanted
                        .iter_mut()
                        .filter(|(n, _)| n.eq_ignore_ascii_case(&name))
                        .collect();
                    if matches.is_empty() {
                        continue;
                    }
                    matches.into_iter().for_each(|m| m.1 = true);
                    selected.push(column);
                }
                if let Some((name, _)) = wanted.iter().find(|(_, seen)| !seen) {
                    return Err(binder_error(format!(
                        "Column \"{name}\" was selected but was not found in the FROM clause"
                    )));
                }
                Ok((selected, None))
            }
            _ => Err(binder_error(
                "COLUMNS expects either a VARCHAR argument (regex) or a LIST of VARCHAR (list of columns)",
            )),
        }
    }

    /// `Binder::ExpandStarExpression`.
    pub(super) fn expand_star_expression(&self, expr: &Expr, result: &mut Vec<Expr>) -> Result<()> {
        let mut expr = expr.clone();
        transform_star_like(&mut expr)?;
        if let ExprKind::Star(star) = &expr.kind
            && is_plain_star(&expr)
        {
            result.extend(self.generate_all_columns(star)?);
            return Ok(());
        }
        let (found, unpacked) = self.find_columns(&mut expr)?;
        let Some(star) = found else {
            result.push(expr);
            return Ok(());
        };
        let (columns, regex) = self.filter_columns(&star, self.generate_all_columns(&star)?)?;
        if unpacked {
            if matches!(
                &expr.kind,
                ExprKind::Operator {
                    op: OperatorType::Unpack,
                    ..
                }
            ) {
                return Err(binder_error(
                    "*COLUMNS not allowed at the root level, use COLUMNS instead",
                ));
            }
            if columns.is_empty() {
                return Err(binder_error(
                    "*COLUMNS() expanded to zero columns, which is not supported in this place",
                ));
            }
            let expanded: Vec<Expr> = columns.iter().map(without_aliases).collect();
            replace_unpacked(&mut expr, &expanded);
            result.push(expr);
            return Ok(());
        }
        for column in &columns {
            let mut new_expr = expr.clone();
            visit_expr_mut(&mut new_expr, &mut |e| {
                if matches!(&e.kind, ExprKind::Star(star) if star.columns) {
                    let alias = std::mem::take(&mut e.alias);
                    *e = column.clone();
                    if !alias.is_empty() {
                        e.alias = alias;
                    }
                }
            });
            if star.columns
                && let Some(name) = resolved_column(column)
            {
                if new_expr.alias.is_empty() {
                    new_expr.alias = name;
                } else {
                    new_expr.alias = replace_columns_alias(&new_expr.alias, &name, regex.as_ref())?;
                }
            }
            result.push(new_expr);
        }
        Ok(())
    }

    /// `Binder::BindWhereStarExpression`.
    pub(super) fn bind_where_star_expression(&self, expr: &Expr) -> Result<Expr> {
        if let ExprKind::Conjunction { and: true, children } = &expr.kind {
            let children = children
                .iter()
                .map(|c| self.bind_where_star_expression(c))
                .collect::<Result<Vec<_>>>()?;
            return Ok(ExprKind::Conjunction { and: true, children }.into());
        }
        if matches!(&expr.kind, ExprKind::Star(star) if !star.columns) {
            return Err(binder_error(
                "STAR expression is not allowed in the WHERE clause. Use COLUMNS(*) instead.",
            ));
        }
        let mut conditions = Vec::new();
        self.expand_star_expression(expr, &mut conditions)?;
        if conditions.is_empty() {
            return Err(binder_error("COLUMNS expansion resulted in empty set of columns"));
        }
        if conditions.len() == 1 {
            return Ok(conditions.remove(0));
        }
        Ok(ExprKind::Conjunction {
            and: true,
            children: conditions,
        }
        .into())
    }
}

/// `ReplaceUnpackedStarExpression`: `f(*COLUMNS(*))` passes every column.
fn replace_unpacked(expr: &mut Expr, columns: &[Expr]) {
    fn is_unpack(expr: &Expr) -> bool {
        matches!(
            &expr.kind,
            ExprKind::Operator {
                op: OperatorType::Unpack,
                ..
            }
        )
    }
    visit_expr_mut(expr, &mut |e| match &mut e.kind {
        ExprKind::Function(function) if function.args.iter().any(|a| is_unpack(&a.expr)) => {
            let mut args = Vec::new();
            for argument in std::mem::take(&mut function.args) {
                if is_unpack(&argument.expr) {
                    args.extend(
                        columns
                            .iter()
                            .map(|c| duckrs_parser::ast::FunctionArgument::positional(c.clone())),
                    );
                } else {
                    args.push(argument);
                }
            }
            function.args = args;
        }
        ExprKind::Operator { children, .. } | ExprKind::Conjunction { children, .. }
            if children.iter().any(is_unpack) =>
        {
            let mut result = Vec::new();
            for child in std::mem::take(children) {
                if is_unpack(&child) {
                    result.extend(columns.iter().cloned());
                } else {
                    result.push(child);
                }
            }
            *children = result;
        }
        _ => {}
    });
}

/// `QueryResult::DeduplicateColumns`: repeated names get `_1`, `_2`, ...
pub(super) fn deduplicate_columns(names: &mut [String]) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names.iter_mut() {
        let key = name.to_lowercase();
        match seen.get(&key).copied() {
            None => {
                seen.insert(key, 1);
            }
            Some(mut count) => {
                let mut candidate = format!("{name}_{count}");
                while seen.contains_key(&candidate.to_lowercase()) {
                    count += 1;
                    candidate = format!("{name}_{count}");
                }
                seen.insert(key, count + 1);
                seen.insert(candidate.to_lowercase(), 1);
                *name = candidate;
            }
        }
    }
}

/// The text of a constant as a value of any type (`Value::ToString`).
pub(super) fn value_text(value: &Value) -> Result<String> {
    if value.is_null() {
        return Ok("NULL".to_string());
    }
    value.try_to_string()
}

pub(super) fn literal_text(literal: &Literal) -> String {
    match literal.kind {
        LiteralKind::Null => "NULL".to_string(),
        _ => literal.text.clone(),
    }
}
