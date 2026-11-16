// Ported from duckdb/src/optimizer/pushdown/pushdown_get.cpp and the table filter part of
// duckdb/src/optimizer/filter_combiner.cpp @ rust-port-baseline
//
//! Moves filters on one column into the scan below them (`TableFilterSet`), where they run on each
//! chunk as it is read. In-memory scans have no zone maps yet, so all this gains is that no
//! operator is left to pass the rows on, and that the columns only they read stay in the scan. It
//! runs late, so that the passes before it see filters as operators, and only takes what can run
//! ahead of the filters that stay: nothing that can fail, unless it is the only filter.

use super::rewriter::{can_throw, contains_barrier, split_conjuncts};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;

/// The columns of the expression, in the query around the plan too, with `outer` for those.
fn columns_of(expr: &BoundExpr, columns: &mut Vec<ColumnBinding>, outer: &mut bool) {
    if let BoundExprKind::ColumnRef { binding, depth } = &expr.kind {
        *outer |= *depth != 0;
        columns.push(*binding);
    }
    for child in expr.children() {
        columns_of(child, columns, outer);
    }
}

/// `TryPushdownGenericExpression`, for a filter on the output of a scan.
fn can_move_into_scan(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile() && !can_throw(expr)
}

/// [`can_move_into_scan`], for a filter that may fail.
fn can_move_ignoring_errors(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile()
}

/// A comparison with the constant on the left turned around, as `FilterCombiner` makes a table
/// filter of the column: `5 < x` is `x > 5`.
fn column_first(mut expr: BoundExpr) -> BoundExpr {
    if let BoundExprKind::Comparison { op, left, right } = &mut expr.kind
        && matches!(left.kind, BoundExprKind::Constant(_))
        && !matches!(right.kind, BoundExprKind::Constant(_))
    {
        std::mem::swap(left, right);
        *op = super::statistics::flip(*op);
    }
    expr
}

/// `TryPushdownPrefixFilter`: `prefix(s, 'abc')` on a column is `s >= 'abc' AND s < 'abd'`, which
/// a scan's zone maps can use.
fn prefix_bounds(expr: &BoundExpr) -> Option<[BoundExpr; 2]> {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return None;
    };
    if !matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") {
        return None;
    }
    let [column, BoundExpr {
        kind: BoundExprKind::Constant(value),
        ..
    }] = children.as_slice()
    else {
        return None;
    };
    if !matches!(column.kind, BoundExprKind::ColumnRef { .. }) || value.is_null() {
        return None;
    }
    let prefix = value.try_to_string().ok()?;
    if prefix.is_empty() || column.return_type.collation().is_some() {
        return None;
    }
    // `Utf8Proc::FindNextLegalUTF8`: the next string of the same length, by its last character
    let mut chars: Vec<char> = prefix.chars().collect();
    let last = chars.pop()?;
    let next = (u32::from(last) + 1..=u32::from(char::MAX)).find_map(char::from_u32)?;
    chars.push(next);
    let upper: String = chars.into_iter().collect();
    let compare = |op, bound: String| {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(column.clone()),
                right: Box::new(BoundExpr::constant(duckrs_common::value::Value::varchar(bound))),
            },
            duckrs_common::types::LogicalType::BOOLEAN,
        )
    };
    Some([
        compare(duckrs_parser::ast::ComparisonOp::GreaterThanOrEqual, prefix),
        compare(duckrs_parser::ast::ComparisonOp::LessThan, upper),
    ])
}

/// Moves the filters directly on scans into them.
pub fn push_into_scans(op: &mut LogicalOperator) {
    for child in &mut op.children {
        push_into_scans(child);
    }
    if let K::LateralJoin { right, .. } = &mut op.kind {
        push_into_scans(right);
    }
    let K::Filter { expressions } = &mut op.kind else {
        return;
    };
    // `MultiFileFunction` has no `filter_pushdown` for CSV and JSON, only `pushdown_complex_filter`, which
    // leaves out files
    if matches!(op.children[0].kind, K::CsvScan { .. } | K::JsonScan { .. }) {
        let bindings = op.children[0].column_bindings();
        let mut conjuncts = Vec::new();
        for expr in std::mem::take(expressions) {
            split_conjuncts(expr, &mut conjuncts);
        }
        match &mut op.children[0].kind {
            K::CsvScan { scan: data, .. } => match std::sync::Arc::get_mut(data) {
                Some(data) => prune_files(data, &bindings, &mut conjuncts),
                None => {
                    let mut copy = super::type_pushdown::clone_scan(data);
                    prune_files(&mut copy, &bindings, &mut conjuncts);
                    *data = std::sync::Arc::new(copy);
                }
            },
            K::JsonScan { scan: data, .. } => prune_files(std::sync::Arc::make_mut(data), &bindings, &mut conjuncts),
            _ => unreachable!("checked above"),
        }
        *expressions = conjuncts;
        if expressions.is_empty() {
            *op = op.children.remove(0);
        }
        return;
    }
    if !matches!(op.children[0].kind, K::Get { .. } | K::ParquetScan { .. }) {
        return;
    }
    let scan = op.children[0].column_bindings();
    let mut conjuncts = Vec::new();
    for expr in std::mem::take(expressions) {
        split_conjuncts(expr, &mut conjuncts);
    }
    let filters = match &mut op.children[0].kind {
        K::Get { filters, .. } => filters,
        // `MultiFileFunction` has `filter_pushdown`; these run as the file is read
        K::ParquetScan { scan: data, .. } => {
            let data = std::sync::Arc::make_mut(data);
            prune_files(data, &scan, &mut conjuncts);
            &mut data.filters
        }
        _ => unreachable!("checked above"),
    };
    let (barred, conjuncts): (Vec<BoundExpr>, Vec<BoundExpr>) = conjuncts.into_iter().partition(contains_barrier);
    for conjunct in conjuncts {
        if can_move_into_scan(&conjunct, &scan) {
            if let Some(bounds) = prefix_bounds(&conjunct) {
                filters.extend(bounds);
                continue;
            }
            filters.push(column_first(conjunct));
        } else {
            expressions.push(conjunct);
        }
    }
    // `PushdownGet`: a filter that can fail goes into the scan too when it is the only one left, as then there is
    // no other filter it would run ahead of
    if let [only] = expressions.as_slice()
        && barred.is_empty()
        && can_move_ignoring_errors(only, &scan)
    {
        let only = expressions.pop().expect("one filter");
        match prefix_bounds(&only) {
            Some(bounds) => filters.extend(bounds),
            None => filters.push(column_first(only)),
        }
    }
    // `PushdownBarrierFilters`: into the scan, after the filters there, only if no other filter stays on top
    for conjunct in barred {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(&conjunct, &mut columns, &mut outer);
        let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]) && scan.contains(&columns[0]);
        if expressions.is_empty() && one_column && !outer && !conjunct.is_volatile() {
            filters.push(conjunct);
        } else {
            expressions.push(conjunct);
        }
    }
    // `TryCreateMultiColumnExpressionFilter`: a filter on several columns of a table stays, and a copy tells the
    // scan which row groups none of its rows pass in
    if let K::Get { zone_filters, .. } = &mut op.children[0].kind {
        for expr in expressions.iter() {
            let mut columns = Vec::new();
            let mut outer = false;
            columns_of(expr, &mut columns, &mut outer);
            let several = columns.iter().any(|c| *c != columns[0]);
            if several && !outer && columns.iter().all(|c| scan.contains(c)) && !expr.is_volatile() {
                zone_filters.push(expr.clone());
            }
        }
    }
    if expressions.is_empty() {
        *op = op.children.remove(0);
    }
}

/// The files of a multi-file scan, with the columns that have one value for each of them.
pub(crate) trait FileList {
    fn paths(&self) -> Vec<String>;
    /// The value of output column `at` in the file at `path`, if it is the same for all its rows.
    fn known_value(&self, at: usize, path: &str) -> Option<Value>;
    /// Keeps the files `keep` says, as filters `file_filters` left them of `total`.
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize);
}

impl FileList for crate::execution::parquet::ParquetScanData {
    fn paths(&self) -> Vec<String> {
        self.files.clone()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::parquet::ParquetScanData::known_value(self, at, path)
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

impl FileList for crate::execution::csv::CsvScanData {
    fn paths(&self) -> Vec<String> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::json_scan::known_value(&self.names, self.filename, &self.hive, &self.virtual_columns, at, path)
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        // the bytes sniffing read are of the first file
        if keep.first() == Some(&false) {
            *self.first_data.lock().expect("not poisoned") = None;
        }
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

impl FileList for crate::execution::json_scan::JsonScanData {
    fn paths(&self) -> Vec<String> {
        self.files.clone()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::json_scan::JsonScanData::known_value(self, at, path)
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

/// `HivePartitioning::ApplyFiltersToFileList`: the filters that only read the hive partitions and the file name
/// are worked out for each file, those of the files they are false for are left out, and the filters that could
/// be worked out for every file are no longer needed.
fn prune_files(scan: &mut impl FileList, bindings: &[ColumnBinding], conjuncts: &mut Vec<BoundExpr>) {
    let paths = scan.paths();
    let Some(first) = paths.first() else {
        return;
    };
    let known: Vec<ColumnBinding> = bindings
        .iter()
        .copied()
        .filter(|b| scan.known_value(b.column_index, first).is_some())
        .collect();
    if known.is_empty() || conjuncts.is_empty() {
        return;
    }
    let mut keep = vec![true; paths.len()];
    let mut file_filters = String::new();
    conjuncts.retain(|conjunct| {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(conjunct, &mut columns, &mut outer);
        // a subquery runs with the query (in a recursive CTE, once per round): not before it
        fn has_subquery(expr: &BoundExpr) -> bool {
            matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
        }
        if columns.is_empty()
            || outer
            || conjunct.is_volatile()
            || has_subquery(conjunct)
            || !columns.iter().all(|c| known.contains(c))
        {
            return true;
        }
        let mut preserved = false;
        let mut applied = false;
        for (path, keep) in paths.iter().zip(keep.iter_mut()) {
            let mut copy = conjunct.clone();
            let mut known_values = true;
            copy.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
                    match scan.known_value(binding.column_index, path) {
                        Some(value) => *e = BoundExpr::constant(value),
                        None => known_values = false,
                    }
                }
            });
            let result = known_values
                .then(|| crate::execution::expression_executor::try_evaluate_scalar(&copy))
                .flatten();
            match result {
                None => preserved = true,
                Some(value) if value.is_null() || value.data() == &duckrs_common::value::ValueData::Bool(false) => {
                    *keep = false;
                    applied = true;
                }
                Some(_) => {}
            }
        }
        // `ExtraOperatorInfo::file_filters`: the filters' texts, one after the other
        if applied {
            file_filters.push_str(&crate::client::explain::filter_text(conjunct));
        }
        preserved
    });
    scan.keep_files(&keep, &file_filters, paths.len());
}
