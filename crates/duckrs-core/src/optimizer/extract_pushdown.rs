// Ported from duckdb/src/optimizer/remove_unused_columns.cpp (`HandleExtractExpression`,
// `WritePushdownExtractColumns`, `RemoveColumnsFromLogicalGet`) @ rust-port-baseline
//
//! Pushdown extract: where a STRUCT or VARIANT column of a table is only read through fields of it
//! (`s.a.b`, `v.x::INTEGER`), the scan outputs those fields in its place, each a column of its own, and
//! what read them reads those. C++ renumbers the scan's columns; here the fields are new positions of
//! `column_ids`, of the column they are of, and the column itself stays for the filters of the scan.

use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K, PushedExtract};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::ValueData;
use std::collections::HashMap;
use std::sync::Arc;

/// One step of a path into a column: a field of a STRUCT by its position, or of a VARIANT by its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Step {
    Field(usize),
    Key(String),
}

/// A path into a column and the cast of what it leads to, if any.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Path {
    steps: Vec<Step>,
    cast: Option<LogicalType>,
}

/// How the operators above a scan read one of its columns.
#[derive(Default)]
struct Uses {
    /// as a whole, somewhere
    whole: bool,
    paths: Vec<Path>,
}

pub fn push_down(plan: &mut LogicalOperator) {
    if !supported(plan) {
        return;
    }
    let mut candidates: HashMap<ColumnBinding, LogicalType> = HashMap::new();
    collect_candidates(plan, &mut candidates);
    if candidates.is_empty() {
        return;
    }
    let mut uses: HashMap<ColumnBinding, Uses> = HashMap::new();
    gather_plan(plan, &candidates, &mut uses);
    // `ReferencedColumn::AddPath`: per column, the paths no other pushed path leads into
    let mut pushed: HashMap<ColumnBinding, Vec<(Path, usize)>> = HashMap::new();
    for (binding, uses) in uses {
        if uses.whole || uses.paths.is_empty() {
            continue;
        }
        let mut paths: Vec<Path> = Vec::new();
        for path in uses.paths {
            if paths.contains(&path) || paths.iter().any(|other| leads_into(other, &path)) {
                continue;
            }
            paths.retain(|other| !leads_into(&path, other));
            paths.push(path);
        }
        pushed.insert(binding, paths.into_iter().map(|path| (path, 0)).collect());
    }
    if pushed.is_empty() {
        return;
    }
    add_positions(plan, &candidates, &mut pushed);
    rewrite_plan(plan, &pushed);
}

/// Whether `prefix` is a path without a cast that `path` goes on from.
fn leads_into(prefix: &Path, path: &Path) -> bool {
    prefix.cast.is_none() && prefix.steps.len() < path.steps.len() && path.steps.starts_with(&prefix.steps)
}

/// Plans with operators that hold bindings or expressions other than those of `expressions_mut` and the
/// join conditions are left as they are.
fn supported(op: &LogicalOperator) -> bool {
    let own = !matches!(
        op.kind,
        K::MatchRecognize { .. }
            | K::ExceptIntersect { .. }
            | K::EmptyResult { .. }
            | K::LateralJoin { .. }
            | K::Insert { .. }
            | K::Delete { .. }
            | K::Update { .. }
            | K::MergeInto { .. }
    );
    own && op.children.iter().all(supported)
}

/// `TableSupportsPushdownExtract`: the STRUCT and VARIANT columns that a scan outputs to what reads them
/// by their binding, and not by position.
fn collect_candidates(op: &LogicalOperator, candidates: &mut HashMap<ColumnBinding, LogicalType>) {
    if let K::Get {
        table,
        table_index,
        column_ids,
        projection: Some(projection),
        extracts,
        ..
    } = &op.kind
        && extracts.iter().all(Option::is_none)
    {
        for &position in projection {
            let Some(column) = table.columns.get(column_ids[position]) else {
                continue;
            };
            if matches!(column.ty.id(), LogicalTypeId::Struct | LogicalTypeId::Variant) && !table.is_generated(&column.name) {
                candidates.insert(ColumnBinding::new(*table_index, position), column.ty.clone());
            }
        }
    }
    // a scan of parquet files: the columns of the files, which the operators above read
    if let K::ParquetScan { table_index, scan } = &op.kind
        && scan.extracts.is_empty()
        && let Some(projected) = &scan.projected
    {
        for (position, ty) in scan.types.iter().enumerate() {
            if projected.get(position).copied().unwrap_or(false)
                && matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Variant)
            {
                candidates.insert(ColumnBinding::new(*table_index, position), ty.clone());
            }
        }
    }
    for child in &op.children {
        collect_candidates(child, candidates);
    }
}

/// The expressions of an operator that read its children, with the conditions of joins.
fn reading_expressions(op: &mut LogicalOperator) -> Vec<&mut BoundExpr> {
    let conditions: Vec<*mut BoundExpr> = match &mut op.kind {
        K::Join { conditions, .. }
        | K::DelimJoin { conditions, .. }
        | K::MarkJoin { conditions, .. }
        | K::AsOfJoin { conditions, .. } => conditions
            .iter_mut()
            .flat_map(|c| [&mut c.left as *mut BoundExpr, &mut c.right as *mut BoundExpr])
            .collect(),
        _ => Vec::new(),
    };
    let mut result = match &op.kind {
        // the filters of a scan read the column itself, which stays
        K::Get { .. } | K::ParquetScan { .. } => Vec::new(),
        _ => op.expressions_mut(),
    };
    // SAFETY: the conditions are fields of the operator apart from those `expressions_mut` gives
    result.extend(conditions.into_iter().map(|c| unsafe { &mut *c }));
    result
}

fn gather_plan(op: &mut LogicalOperator, candidates: &HashMap<ColumnBinding, LogicalType>, uses: &mut HashMap<ColumnBinding, Uses>) {
    for expr in reading_expressions(op) {
        gather(expr, candidates, uses, false);
    }
    for child in &mut op.children {
        gather_plan(child, candidates, uses);
    }
}

/// The column an expression extracts a field of, and the path to it (`HandleExtractRecursive`).
fn extract_path(expr: &BoundExpr) -> Option<(ColumnBinding, Vec<Step>)> {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return None;
    };
    if children.len() != 2 {
        return None;
    }
    let BoundExprKind::Constant(key) = &children[1].kind else {
        return None;
    };
    let step = match (function.name.as_str(), children[0].return_type.id()) {
        ("struct_extract", LogicalTypeId::Struct) => Step::Field(usize::try_from(key.as_i128()?).ok()?),
        ("variant_extract", LogicalTypeId::Variant) => match key.data() {
            // a VARIANT is only pushed into by key, not into an ARRAY by index
            ValueData::Bytes(bytes) if !key.logical_type().is_integral() => {
                Step::Key(String::from_utf8(bytes.clone()).ok()?)
            }
            _ => return None,
        },
        _ => return None,
    };
    let (binding, mut steps) = match &children[0].kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => (*binding, Vec::new()),
        _ => extract_path(&children[0])?,
    };
    steps.push(step);
    Some((binding, steps))
}

/// `BaseColumnPruner::VisitExpression`: the paths that `expr` reads columns through, and the columns it
/// reads as a whole.
fn gather(
    expr: &BoundExpr,
    candidates: &HashMap<ColumnBinding, LogicalType>,
    uses: &mut HashMap<ColumnBinding, Uses>,
    inside_try: bool,
) {
    if let BoundExprKind::Operator { op, children } = &expr.kind
        && matches!(op, BoundOperator::Try | BoundOperator::HasError)
    {
        for child in children {
            gather(child, candidates, uses, true);
        }
        return;
    }
    // a cast of a field is pushed with it, unless it is a TRY_CAST or inside TRY, or of a VARIANT to JSON,
    // which differs from the cast of the typed value
    if let BoundExprKind::Cast { child, try_cast: false } = &expr.kind
        && !inside_try
        && let Some((binding, steps)) = extract_path(child)
        && let Some(column_type) = candidates.get(&binding)
    {
        let json = crate::function::json::is_json_type(&expr.return_type);
        if !(column_type.id() == LogicalTypeId::Variant && json) {
            let path = Path {
                steps,
                cast: Some(expr.return_type.clone()),
            };
            uses.entry(binding).or_default().paths.push(path);
            return;
        }
    }
    if let Some((binding, steps)) = extract_path(expr)
        && candidates.contains_key(&binding)
    {
        uses.entry(binding).or_default().paths.push(Path { steps, cast: None });
        return;
    }
    if let BoundExprKind::ColumnRef { binding, .. } = &expr.kind
        && candidates.contains_key(binding)
    {
        uses.entry(*binding).or_default().whole = true;
    }
    for child in expr.children() {
        gather(child, candidates, uses, inside_try);
    }
}

/// `ConstructStructExtractFromPath`: the expression that gives the field from the column at `Reference(0)`.
fn path_expression(column_type: &LogicalType, path: &Path) -> Option<(BoundExpr, Vec<String>)> {
    path_extract(column_type, path).map(|extract| (extract.expression, extract.names))
}

/// The [`PushedExtract`] of `path`, with the keys of a path into a VARIANT.
fn path_extract(column_type: &LogicalType, path: &Path) -> Option<PushedExtract> {
    let (expression, names) = path_expression_of(column_type, path)?;
    let keys: Option<Vec<String>> = path
        .steps
        .iter()
        .map(|step| match step {
            Step::Key(key) => Some(key.clone()),
            Step::Field(_) => None,
        })
        .collect();
    let variant_path = keys.filter(|_| column_type.id() == LogicalTypeId::Variant).map(|keys| {
        let field = BoundExpr::new(BoundExprKind::Reference(0), column_type.clone());
        let rest = match &path.cast {
            Some(cast) => field.cast_to(cast, false),
            None => field,
        };
        (keys, rest)
    });
    Some(PushedExtract {
        names,
        expression,
        variant_path,
    })
}

fn path_expression_of(column_type: &LogicalType, path: &Path) -> Option<(BoundExpr, Vec<String>)> {
    let mut expr = BoundExpr::new(BoundExprKind::Reference(0), column_type.clone());
    let mut names = Vec::with_capacity(path.steps.len());
    for step in &path.steps {
        expr = match step {
            Step::Field(index) => {
                names.push(expr.return_type.struct_children().get(*index)?.0.clone());
                crate::function::nested::struct_extract_at_index(expr, *index)?
            }
            Step::Key(key) => {
                names.push(key.clone());
                let key = BoundExpr::constant(duckrs_common::value::Value::varchar(key.as_str()));
                crate::function::variant::bind_extract(expr, key).ok()?
            }
        };
    }
    if let Some(cast) = &path.cast {
        expr = expr.cast_to(cast, false);
    }
    Some((expr, names))
}

/// The positions of the scans that output the pushed paths, in `pushed`.
fn add_positions(
    op: &mut LogicalOperator,
    candidates: &HashMap<ColumnBinding, LogicalType>,
    pushed: &mut HashMap<ColumnBinding, Vec<(Path, usize)>>,
) {
    if let K::Get {
        table_index,
        column_ids,
        projection: Some(projection),
        extracts,
        ..
    } = &mut op.kind
    {
        let table_index = *table_index;
        let positions: Vec<usize> = projection.clone();
        for position in positions {
            let binding = ColumnBinding::new(table_index, position);
            let Some(paths) = pushed.get_mut(&binding) else {
                continue;
            };
            let column_type = &candidates[&binding];
            // all of them, or none: a path that can't be rebuilt keeps the column as it is
            let built: Option<Vec<PushedExtract>> =
                paths.iter().map(|(path, _)| path_extract(column_type, path)).collect();
            let Some(built) = built else {
                pushed.remove(&binding);
                continue;
            };
            extracts.resize(column_ids.len(), None);
            for (extract, (_, at)) in built.into_iter().zip(paths.iter_mut()) {
                *at = column_ids.len();
                column_ids.push(column_ids[position]);
                extracts.push(Some(Arc::new(extract)));
                projection.push(*at);
            }
            projection.retain(|&p| p != position);
        }
    }
    if let K::ParquetScan { table_index, scan } = &mut op.kind {
        let table_index = *table_index;
        let scan = Arc::make_mut(scan);
        for position in 0..scan.types.len() {
            let binding = ColumnBinding::new(table_index, position);
            let Some(paths) = pushed.get_mut(&binding) else {
                continue;
            };
            let column_type = &candidates[&binding];
            let built: Option<Vec<(BoundExpr, Vec<String>)>> =
                paths.iter().map(|(path, _)| path_expression(column_type, path)).collect();
            let Some(built) = built else {
                pushed.remove(&binding);
                continue;
            };
            for ((expression, names), (_, at)) in built.into_iter().zip(paths.iter_mut()) {
                *at = scan.output_types().len();
                let extract = PushedExtract {
                    names,
                    expression,
                    variant_path: None,
                };
                scan.extracts.push((position, Arc::new(extract)));
                if let Some(projected) = &mut scan.projected {
                    projected.push(true);
                }
            }
            if let Some(projected) = &mut scan.projected {
                projected[position] = false;
            }
        }
    }
    for child in &mut op.children {
        add_positions(child, candidates, pushed);
    }
}

fn rewrite_plan(op: &mut LogicalOperator, pushed: &HashMap<ColumnBinding, Vec<(Path, usize)>>) {
    if let K::ColumnSelect { keep } = &mut op.kind {
        let mut kept = Vec::with_capacity(keep.len());
        for binding in keep.drain(..) {
            match pushed.get(&binding) {
                Some(paths) => kept.extend(paths.iter().map(|(_, at)| ColumnBinding::new(binding.table_index, *at))),
                None => kept.push(binding),
            }
        }
        *keep = kept;
    }
    for expr in reading_expressions(op) {
        rewrite(expr, pushed, false);
    }
    for child in &mut op.children {
        rewrite_plan(child, pushed);
    }
}

/// `WritePushdownExtractColumns`: what reads a pushed path, or one it leads into, reads the scan's column of it.
fn rewrite(expr: &mut BoundExpr, pushed: &HashMap<ColumnBinding, Vec<(Path, usize)>>, inside_try: bool) {
    if let BoundExprKind::Operator { op, children } = &mut expr.kind
        && matches!(op, BoundOperator::Try | BoundOperator::HasError)
    {
        for child in children {
            rewrite(child, pushed, true);
        }
        return;
    }
    if let BoundExprKind::Cast { child, try_cast: false } = &expr.kind
        && !inside_try
        && let Some((binding, steps)) = extract_path(child)
        && let Some(paths) = pushed.get(&binding)
    {
        let full = Path {
            steps: steps.clone(),
            cast: Some(expr.return_type.clone()),
        };
        if let Some((_, at)) = paths.iter().find(|(path, _)| *path == full) {
            *expr = column(binding, *at, expr.return_type.clone(), expr);
            return;
        }
    }
    if let Some((binding, steps)) = extract_path(expr)
        && let Some(paths) = pushed.get(&binding)
        && let Some((path, at)) = paths
            .iter()
            .find(|(path, _)| path.cast.is_none() && steps.starts_with(&path.steps))
    {
        // the extracts beyond the pushed path stay on the scan's column
        let depth = steps.len() - path.steps.len();
        let mut target: &mut BoundExpr = expr;
        for _ in 0..depth {
            let BoundExprKind::Function { children, .. } = &mut target.kind else {
                unreachable!("an extract")
            };
            target = &mut children[0];
        }
        *target = column(binding, *at, target.return_type.clone(), target);
        return;
    }
    for child in expr.children_mut() {
        rewrite(child, pushed, inside_try);
    }
}

/// The scan's column at `at`, named as the column whose field it is (`colref.Copy()`).
fn column(binding: ColumnBinding, at: usize, ty: LogicalType, expr: &BoundExpr) -> BoundExpr {
    fn base(expr: &BoundExpr) -> &BoundExpr {
        match &expr.kind {
            BoundExprKind::Function { children, .. } if !children.is_empty() => base(&children[0]),
            BoundExprKind::Cast { child, .. } => base(child),
            _ => expr,
        }
    }
    let alias = base(expr).alias.clone();
    BoundExpr::column_ref(ColumnBinding::new(binding.table_index, at), ty, &alias)
}
