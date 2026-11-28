//! `ScalarFnPushdown`: `strlen(col)` or `octet_length(col)` of a column of a Parquet scan, when it
//! is the only way the plan uses the column, is pushed into the scan, which then reads the byte
//! lengths of the column's values in place of the values.
//!
//! As in C++ (`ScalarFnCollect` / `ScalarFnReplace` through `PushdownOptimize`), a candidate is a
//! function of one argument that is the column; any other use of the column (bare, in a function
//! of several arguments, in a cast, or in a different function) is a conflict. Bindings resolve
//! one level through a projection that only forwards columns of the scan (a view's `SELECT col`).

use super::rewriter::expressions_equal;
use super::type_pushdown::{for_each_expression, inner_plans};
use crate::execution::parquet::{ParquetScanData, reads_byte_length};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use pintail_common::types::{LogicalType, LogicalTypeId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `GetAnalysis`: for each column of a scan, the expression all its uses agree on, or `None` when
/// some use disagrees.
struct Analysis {
    scan: Arc<ParquetScanData>,
    exprs: HashMap<usize, Option<BoundExpr>>,
}

/// `Projections`: a projection that only forwards columns of a scan, by its table index, with the
/// scan column each of its outputs is.
type Projections = HashMap<usize, Vec<ColumnBinding>>;

/// `ScalarFnPushdown::Optimize`.
pub fn push_down_scalar_functions(plan: &mut LogicalOperator) {
    let mut analyses: HashMap<usize, Analysis> = HashMap::new();
    let mut projections = Projections::new();
    find_scans_and_projections(plan, &mut analyses, &mut projections);
    if analyses.is_empty() {
        return;
    }
    let mut collect = Collect {
        analyses: &mut analyses,
        projections: &projections,
    };
    collect.visit_operator(plan);

    let mut pushed: HashMap<usize, (Arc<ParquetScanData>, HashSet<usize>)> = HashMap::new();
    for (table_index, analysis) in &analyses {
        let columns: HashSet<usize> = analysis
            .exprs
            .iter()
            .filter_map(|(column, expr)| Some((*column, expr.as_ref()?)))
            .filter(|(column, expr)| can_push(&analysis.scan, *column, expr))
            .map(|(column, _)| column)
            .collect();
        if columns.is_empty() {
            continue;
        }
        let mut scan = ParquetScanData::clone(&analysis.scan);
        for column in &columns {
            scan.types[*column] = LogicalType::BIGINT;
            scan.byte_lengths.push(*column);
        }
        scan.byte_lengths.sort_unstable();
        pushed.insert(*table_index, (Arc::new(scan), columns));
    }
    if pushed.is_empty() {
        return;
    }
    replace_scans(plan, &pushed);
    let pushed: HashMap<usize, HashSet<usize>> = pushed
        .into_iter()
        .map(|(table, (_, columns))| (table, columns))
        .collect();
    replace(plan, &pushed, &projections);
}

/// `ParquetScanFunction::ProjectionExpressionPushdown`.
fn can_push(scan: &ParquetScanData, column: usize, expr: &BoundExpr) -> bool {
    let BoundExprKind::Function { function, .. } = &expr.kind else {
        return false;
    };
    let wanted = match function.name.as_str() {
        "strlen" => LogicalTypeId::Varchar,
        "octet_length" => LogicalTypeId::Blob,
        _ => return false,
    };
    // not with a schema of its own, files unified by name, or more than one file
    if scan.schema.is_some() || scan.union_by_name || scan.files.len() != 1 || !scan.table_columns.is_empty() {
        return false;
    }
    // only a column read from the file as it is (not a hive partition)
    if column >= scan.names.len()
        || scan.types[column].id() != wanted
        || scan.hive_overrides.iter().any(|(at, _)| *at == column)
    {
        return false;
    }
    // a declared column the file does not have as declared is not read as it is
    if !crate::execution::parquet::bound_column_matches(scan, &scan.files[0], column) {
        return false;
    }
    // a filter pushed into the scan needs the column itself
    if scan
        .filters
        .iter()
        .chain(&scan.optional_filters)
        .any(|filter| references(filter, column))
    {
        return false;
    }
    reads_byte_length(scan, column)
}

/// Whether a filter of a scan reads its column `column` (its column references are all of the scan).
fn references(expr: &BoundExpr, column: usize) -> bool {
    let reads = match &expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => binding.column_index == column,
        BoundExprKind::Reference(at) => *at == column,
        _ => false,
    };
    reads || expr.children().into_iter().any(|child| references(child, column))
}

/// `FindGetsAndProjections`.
fn find_scans_and_projections(
    op: &mut LogicalOperator,
    analyses: &mut HashMap<usize, Analysis>,
    projections: &mut Projections,
) {
    match &op.kind {
        LogicalOperatorKind::ParquetScan { table_index, scan } => {
            analyses.entry(*table_index).or_insert_with(|| Analysis {
                scan: Arc::clone(scan),
                exprs: HashMap::new(),
            });
        }
        LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } if !expressions.is_empty() && op.children.len() == 1 => {
            let passthrough: Option<Vec<ColumnBinding>> = expressions
                .iter()
                .map(|e| match &e.kind {
                    BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
                    _ => None,
                })
                .collect();
            if let Some(bindings) = passthrough {
                // a LIMIT can sit between the projection and the scan
                let mut child = &op.children[0];
                if matches!(child.kind, LogicalOperatorKind::Limit { .. }) && child.children.len() == 1 {
                    child = &child.children[0];
                }
                if matches!(child.kind, LogicalOperatorKind::ParquetScan { .. }) {
                    projections.insert(*table_index, bindings);
                }
            }
        }
        _ => {}
    }
    for child in inner_plans(op) {
        find_scans_and_projections(child, analyses, projections);
    }
}

/// `Resolve`: the scan and column a binding is, directly or through a forwarding projection. Only
/// columns read from the files resolve (not the file name, row numbers or virtual columns).
fn resolve(
    binding: ColumnBinding,
    analyses: &HashMap<usize, Analysis>,
    projections: &Projections,
) -> Option<(usize, usize)> {
    let real = |table: usize, column: usize| {
        analyses
            .get(&table)
            .is_some_and(|analysis| column < analysis.scan.names.len())
            .then_some((table, column))
    };
    if analyses.contains_key(&binding.table_index) {
        return real(binding.table_index, binding.column_index);
    }
    let inner = projections.get(&binding.table_index)?.get(binding.column_index)?;
    real(inner.table_index, inner.column_index)
}

/// The column of a function of one argument that is a column, `fn(col)`.
fn single_column_argument(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::Function { children, .. } if children.len() == 1 => match &children[0].kind {
            BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
            _ => None,
        },
        _ => None,
    }
}

/// `ScalarFnCollect`.
struct Collect<'a> {
    analyses: &'a mut HashMap<usize, Analysis>,
    projections: &'a Projections,
}

impl Collect<'_> {
    fn visit_operator(&mut self, op: &mut LogicalOperator) {
        // the columns a forwarding projection passes on are no use of them; a scan's filters are
        // checked when pushing
        let skip = match &op.kind {
            LogicalOperatorKind::Projection { table_index, .. } => self.projections.contains_key(table_index),
            LogicalOperatorKind::ParquetScan { .. } => true,
            _ => false,
        };
        for child in inner_plans(op) {
            self.visit_operator(child);
        }
        if !skip {
            for_each_expression(op, &mut |expr| self.visit_expression(expr));
        }
    }

    fn conflict(&mut self, binding: ColumnBinding) {
        if let Some((table, column)) = resolve(binding, self.analyses, self.projections) {
            self.analyses
                .get_mut(&table)
                .expect("resolved")
                .exprs
                .insert(column, None);
        }
    }

    fn visit_expression(&mut self, expr: &mut BoundExpr) {
        if let Some(binding) = single_column_argument(expr)
            && let Some((table, column)) = resolve(binding, self.analyses, self.projections)
        {
            let exprs = &mut self.analyses.get_mut(&table).expect("resolved").exprs;
            match exprs.get_mut(&column) {
                None => {
                    exprs.insert(column, Some(expr.clone()));
                }
                Some(found) => {
                    if found.as_ref().is_some_and(|found| !same_function(found, expr)) {
                        *found = None;
                    }
                }
            }
            return;
        }
        match &mut expr.kind {
            BoundExprKind::ColumnRef { binding, .. } => self.conflict(*binding),
            BoundExprKind::Subquery { subquery, .. } => {
                // a subquery that runs as a plan of its own can read the columns too
                let mut plan = subquery.plan.clone();
                self.conflicts_in(&mut plan);
                for child in expr.children_mut() {
                    self.visit_expression(child);
                }
            }
            _ => {
                for child in expr.children_mut() {
                    self.visit_expression(child);
                }
            }
        }
    }

    /// Every column of a scan that `plan` references is used in a way that can't be pushed.
    fn conflicts_in(&mut self, plan: &mut LogicalOperator) {
        let mut bindings = Vec::new();
        plan.visit_expressions(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, .. } = &e.kind {
                bindings.push(*binding);
            }
        });
        for binding in bindings {
            self.conflict(binding);
        }
    }
}

/// `Expression::Equals` of two `fn(col)`: the same function, whichever binding (the scan's or a
/// forwarding projection's) names the column.
fn same_function(a: &BoundExpr, b: &BoundExpr) -> bool {
    match (&a.kind, &b.kind) {
        (BoundExprKind::Function { function: f, .. }, BoundExprKind::Function { function: g, .. }) => {
            if f.name != g.name || a.return_type != b.return_type {
                return false;
            }
            // compare with the arguments aside: both are the same column
            let strip = |e: &BoundExpr| {
                let mut e = e.clone();
                for child in e.children_mut() {
                    *child = BoundExpr::new(BoundExprKind::Reference(0), child.return_type.clone());
                }
                e
            };
            expressions_equal(&strip(a), &strip(b))
        }
        _ => false,
    }
}

/// Puts the scans that read byte lengths into the plan.
fn replace_scans(op: &mut LogicalOperator, pushed: &HashMap<usize, (Arc<ParquetScanData>, HashSet<usize>)>) {
    if let LogicalOperatorKind::ParquetScan { table_index, scan } = &mut op.kind
        && let Some((new_scan, _)) = pushed.get(table_index)
    {
        *scan = Arc::clone(new_scan);
    }
    for child in inner_plans(op) {
        replace_scans(child, pushed);
    }
}

/// `ScalarFnReplace`: the functions of a pushed column become the column, which is BIGINT now.
fn replace(op: &mut LogicalOperator, pushed: &HashMap<usize, HashSet<usize>>, projections: &Projections) {
    for child in inner_plans(op) {
        replace(child, pushed, projections);
    }
    if matches!(op.kind, LogicalOperatorKind::ParquetScan { .. }) {
        return;
    }
    let is_pushed = |binding: ColumnBinding| -> bool {
        let (table, column) = if pushed.contains_key(&binding.table_index) {
            (binding.table_index, binding.column_index)
        } else {
            match projections
                .get(&binding.table_index)
                .and_then(|p| p.get(binding.column_index))
            {
                Some(inner) => (inner.table_index, inner.column_index),
                None => return false,
            }
        };
        pushed.get(&table).is_some_and(|columns| columns.contains(&column))
    };
    for_each_expression(op, &mut |expr| replace_expression(expr, &is_pushed));
}

fn replace_expression(expr: &mut BoundExpr, is_pushed: &impl Fn(ColumnBinding) -> bool) {
    if let Some(binding) = single_column_argument(expr)
        && is_pushed(binding)
    {
        let BoundExprKind::Function { children, .. } = &mut expr.kind else {
            unreachable!("a function");
        };
        let mut column = children.pop().expect("one argument");
        column.return_type = LogicalType::BIGINT;
        if !expr.alias.is_empty() {
            column.alias = std::mem::take(&mut expr.alias);
        }
        *expr = column;
        return;
    }
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => {
            if is_pushed(*binding) {
                expr.return_type = LogicalType::BIGINT;
            }
        }
        _ => {
            for child in expr.children_mut() {
                replace_expression(child, is_pushed);
            }
        }
    }
}
