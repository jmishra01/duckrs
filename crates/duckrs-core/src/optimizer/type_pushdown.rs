//! `TypePushdown`: a cast of a column of a CSV scan that is the only way the plan uses the
//! column is pushed into the scan, which then converts the column to that type itself. This way
//! `ignore_errors` covers the conversions that fail, and the reader reports them.
//!
//! As in C++, a cast is only a candidate when it is a direct output of a projection that only
//! forwards columns and casts, with nothing but projections between it and the scan. Bindings
//! resolve one level through a projection that only forwards columns of the scan (a view's
//! `SELECT col`).

use crate::execution::csv::CsvScanData;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `GetAnalysis`: for each column of a scan, the cast all its uses agree on, or `None` when some
/// use disagrees (`col_to_expr` with a null expression).
struct Analysis {
    scan: Arc<CsvScanData>,
    casts: HashMap<usize, Option<(LogicalType, bool)>>,
}

/// `Projections`: a projection that only forwards columns of a scan, by its table index, with the
/// scan column each of its outputs is.
type Projections = HashMap<usize, Vec<ColumnBinding>>;

/// `TypePushdown::Optimize`.
pub fn push_down_types(plan: &mut LogicalOperator) {
    let mut analyses: HashMap<usize, Analysis> = HashMap::new();
    let mut projections = Projections::new();
    find_scans_and_projections(plan, &mut analyses, &mut projections);
    if analyses.is_empty() {
        return;
    }
    let mut collect = Collect {
        analyses: &mut analyses,
        projections: &projections,
        top_level: HashSet::new(),
    };
    collect.visit_operator(plan);

    // `PushdownProjectionExpression`: only columns read from the files take a type (not the
    // filename, hive partition or virtual columns)
    let mut pushed: HashMap<usize, (Arc<CsvScanData>, HashMap<usize, LogicalType>)> = HashMap::new();
    for (table_index, analysis) in &analyses {
        let types: HashMap<usize, LogicalType> = analysis
            .casts
            .iter()
            .filter_map(|(column, cast)| Some((*column, cast.as_ref()?.0.clone())))
            .filter(|(column, _)| *column < analysis.scan.types.len())
            .collect();
        if types.is_empty() {
            continue;
        }
        let mut scan = clone_scan(&analysis.scan);
        for (column, ty) in &types {
            scan.types[*column] = ty.clone();
        }
        pushed.insert(*table_index, (Arc::new(scan), types));
    }
    if pushed.is_empty() {
        return;
    }
    replace_scans(plan, &pushed);
    let pushed: HashMap<usize, HashMap<usize, LogicalType>> =
        pushed.into_iter().map(|(table, (_, types))| (table, types)).collect();
    replace(plan, &pushed, &projections);
}

/// A copy of the scan's data, to change its types in.
fn clone_scan(scan: &CsvScanData) -> CsvScanData {
    CsvScanData {
        files: scan.files.clone(),
        options: scan.options.clone(),
        names: scan.names.clone(),
        types: scan.types.clone(),
        parsing: scan.parsing.clone(),
        hive: scan.hive.clone(),
        force_not_null: scan.force_not_null.clone(),
        manual_types: scan.manual_types.clone(),
        columns_set: scan.columns_set,
        mismatch: scan.mismatch.clone(),
        union_by_name: scan.union_by_name,
        filename: scan.filename,
        virtual_columns: scan.virtual_columns.clone(),
        rejects: scan.rejects.clone(),
        first_data: std::sync::Mutex::new(scan.first_data.lock().map(|data| data.clone()).unwrap_or(None)),
    }
}

/// The plans this plan runs as part of it, which hold operators the way children do.
fn inner_plans(op: &mut LogicalOperator) -> Vec<&mut LogicalOperator> {
    let mut result: Vec<&mut LogicalOperator> = op.children.iter_mut().collect();
    match &mut op.kind {
        LogicalOperatorKind::RecursiveCte { recursive: inner, .. }
        | LogicalOperatorKind::LateralJoin { right: inner, .. } => result.push(inner),
        _ => {}
    }
    result
}

/// Calls `f` on each expression of an operator (not of the operators below), with the conditions
/// of its joins.
fn for_each_expression(op: &mut LogicalOperator, f: &mut impl FnMut(&mut BoundExpr)) {
    for expr in op.expressions_mut() {
        f(expr);
    }
    if let LogicalOperatorKind::Join { conditions, .. }
    | LogicalOperatorKind::DelimJoin { conditions, .. }
    | LogicalOperatorKind::MarkJoin { conditions, .. }
    | LogicalOperatorKind::AsOfJoin { conditions, .. } = &mut op.kind
    {
        for condition in conditions {
            f(&mut condition.left);
            f(&mut condition.right);
        }
    }
}

/// `FindGetsAndProjections`.
fn find_scans_and_projections(
    op: &mut LogicalOperator,
    analyses: &mut HashMap<usize, Analysis>,
    projections: &mut Projections,
) {
    match &op.kind {
        LogicalOperatorKind::CsvScan { table_index, scan } => {
            analyses.entry(*table_index).or_insert_with(|| Analysis {
                scan: Arc::clone(scan),
                casts: HashMap::new(),
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
                if matches!(child.kind, LogicalOperatorKind::CsvScan { .. }) {
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

/// `Resolve`: the scan and column a binding is, directly or through a forwarding projection.
fn resolve(
    binding: ColumnBinding,
    analyses: &HashMap<usize, Analysis>,
    projections: &Projections,
) -> Option<(usize, usize)> {
    if analyses.contains_key(&binding.table_index) {
        return Some((binding.table_index, binding.column_index));
    }
    let inner = projections.get(&binding.table_index)?.get(binding.column_index)?;
    analyses
        .contains_key(&inner.table_index)
        .then_some((inner.table_index, inner.column_index))
}

/// `ReachesPushdownGet`: a chain of single children, through projections only, ends at a scan.
fn reaches_scan(op: &LogicalOperator) -> bool {
    let mut current = op;
    while current.children.len() == 1 {
        current = &current.children[0];
        match current.kind {
            LogicalOperatorKind::CsvScan { .. } => return true,
            LogicalOperatorKind::Projection { .. } => continue,
            _ => return false,
        }
    }
    false
}

fn is_cast(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Cast { .. })
}

/// `CastCollect`.
struct Collect<'a> {
    analyses: &'a mut HashMap<usize, Analysis>,
    projections: &'a Projections,
    /// the casts that are outputs of a clean projection: only they start a candidate
    top_level: HashSet<*const BoundExpr>,
}

impl Collect<'_> {
    fn visit_operator(&mut self, op: &mut LogicalOperator) {
        if let LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } = &op.kind
        {
            let clean = reaches_scan(op)
                && expressions
                    .iter()
                    .all(|e| matches!(e.kind, BoundExprKind::ColumnRef { .. }) || is_cast(e));
            if clean {
                for expr in expressions.iter().filter(|e| is_cast(e)) {
                    self.top_level.insert(expr as *const BoundExpr);
                }
            }
            // the columns a forwarding projection passes on are no use of them
            if self.projections.contains_key(table_index) {
                for child in inner_plans(op) {
                    self.visit_operator(child);
                }
                return;
            }
        }
        for child in inner_plans(op) {
            self.visit_operator(child);
        }
        for_each_expression(op, &mut |expr| self.visit_expression(expr));
    }

    fn conflict(&mut self, binding: ColumnBinding) {
        if let Some((table, column)) = resolve(binding, self.analyses, self.projections) {
            self.analyses
                .get_mut(&table)
                .expect("resolved")
                .casts
                .insert(column, None);
        }
    }

    fn visit_expression(&mut self, expr: &mut BoundExpr) {
        let address = expr as *const BoundExpr;
        let return_type = expr.return_type.clone();
        match &mut expr.kind {
            BoundExprKind::ColumnRef { binding, .. } => self.conflict(*binding),
            BoundExprKind::Cast { child, try_cast } => {
                let BoundExprKind::ColumnRef { binding, .. } = &child.kind else {
                    self.visit_expression(child);
                    return;
                };
                let Some((table, column)) = resolve(*binding, self.analyses, self.projections) else {
                    return;
                };
                let top_level = self.top_level.contains(&address);
                let wanted = (return_type, *try_cast);
                let casts = &mut self.analyses.get_mut(&table).expect("resolved").casts;
                match casts.get_mut(&column) {
                    None => {
                        if top_level {
                            casts.insert(column, Some(wanted));
                        }
                    }
                    Some(found) => {
                        if found.as_ref() != Some(&wanted) {
                            *found = None;
                        }
                    }
                }
            }
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

    /// Every column of a scan that `plan` references is used without a cast it can push.
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

/// Puts the scans with their new types into the plan.
fn replace_scans(op: &mut LogicalOperator, pushed: &HashMap<usize, (Arc<CsvScanData>, HashMap<usize, LogicalType>)>) {
    if let LogicalOperatorKind::CsvScan { table_index, scan } = &mut op.kind {
        if let Some((new_scan, _)) = pushed.get(table_index) {
            *scan = Arc::clone(new_scan);
        }
    }
    for child in inner_plans(op) {
        replace_scans(child, pushed);
    }
}

/// `CastReplace`: the casts of a pushed column become the column, which has the cast's type now.
fn replace(op: &mut LogicalOperator, pushed: &HashMap<usize, HashMap<usize, LogicalType>>, projections: &Projections) {
    for child in inner_plans(op) {
        replace(child, pushed, projections);
    }
    let pushed_type = |binding: ColumnBinding| -> Option<LogicalType> {
        let (table, column) = if pushed.contains_key(&binding.table_index) {
            (binding.table_index, binding.column_index)
        } else {
            let inner = projections.get(&binding.table_index)?.get(binding.column_index)?;
            (inner.table_index, inner.column_index)
        };
        pushed.get(&table)?.get(&column).cloned()
    };
    for_each_expression(op, &mut |expr| replace_expression(expr, &pushed_type));
}

fn replace_expression(expr: &mut BoundExpr, pushed_type: &impl Fn(ColumnBinding) -> Option<LogicalType>) {
    match &mut expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => {
            if let Some(ty) = pushed_type(*binding) {
                expr.return_type = ty;
            }
        }
        BoundExprKind::Cast { child, .. } => {
            if let BoundExprKind::ColumnRef { binding, .. } = &child.kind {
                if let Some(ty) = pushed_type(*binding) {
                    let mut column =
                        std::mem::replace(child.as_mut(), BoundExpr::new(BoundExprKind::Reference(0), ty.clone()));
                    column.return_type = ty;
                    if !expr.alias.is_empty() {
                        column.alias = std::mem::take(&mut expr.alias);
                    }
                    *expr = column;
                }
                return;
            }
            replace_expression(child, pushed_type);
        }
        _ => {
            for child in expr.children_mut() {
                replace_expression(child, pushed_type);
            }
        }
    }
}
