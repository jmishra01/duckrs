// Ported from duckdb/src/optimizer/scalar_aggregate_merger.cpp @ rust-port-baseline
//
//! `ScalarAggregateMerger` (`SCALAR_AGGREGATE_MERGE`): sibling leaves of a cross product that are
//! ungrouped aggregates over filters over the same table(s) become one aggregate over one scan,
//! each aggregate FILTERed to the rows of its branch. `SELECT (SELECT count(*) FROM t WHERE a < 5),
//! (SELECT sum(b) FROM t WHERE a >= 5)` reads `t` once.
//!
//! The scans matched are of catalog tables, `read_parquet`, `read_csv` and `read_json`.

use super::common_subplan::{max_table_index, visit_bindings};
use super::rewriter::{can_throw, expressions_equal, rewrite_expression};
use crate::catalog::TableCatalogEntry;
use crate::execution::parquet::ParquetScanData;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{
    COLUMN_IDENTIFIER_ROW_ID, COLUMN_IDENTIFIER_ROW_NUMBER, LogicalOperator, LogicalOperatorKind as K,
};
use duckrs_common::types::LogicalType;
use duckrs_parser::ast::ComparisonOp;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// What a scan reads.
#[derive(Clone)]
enum Relation {
    Table(Arc<TableCatalogEntry>),
    /// `read_parquet`, whose output columns are its bindings
    Parquet(Arc<ParquetScanData>),
    /// `read_csv` and `read_json`, by what tells two calls apart
    File(String),
}

/// One scan of a branch: what it reads, its table index and columns.
#[derive(Clone)]
struct Scan {
    relation: Relation,
    table_index: usize,
    column_ids: Vec<usize>,
    /// a scan that drops the rows it fails to parse (`ignore_errors`)
    ignore_errors: bool,
    /// the positions in `column_ids` that the branch reads
    read: BTreeSet<usize>,
}

/// `ScalarAggregateMerger::Branch`: `[projection ->] ungrouped aggregate -> filters -> scan(s)`.
struct Branch {
    leaf: usize,
    projection: Option<(usize, Vec<BoundExpr>)>,
    aggregate_index: usize,
    aggregates: Vec<BoundAggregate>,
    grouping_sets: Vec<BTreeSet<usize>>,
    /// the plan under the filters
    relation: LogicalOperator,
    scans: Vec<Scan>,
    conjuncts: Vec<BoundExpr>,
}

/// `MergeTarget`: a scan of the first branch, and the columns it gains for the others.
struct Target {
    scan: Scan,
    added: Vec<(usize, ColumnBinding)>,
}

struct Merger {
    threshold: u64,
    next_index: usize,
    replacements: HashMap<ColumnBinding, ColumnBinding>,
}

/// `ScalarAggregateMerger::Optimize`.
pub fn merge(mut plan: LogicalOperator, threshold: u64) -> LogicalOperator {
    // rebuilding a cross-product tree would move the positions a projection map names
    if has_column_select(&plan) {
        return plan;
    }
    let mut merger = Merger {
        threshold,
        next_index: max_table_index(&plan) + 1,
        replacements: HashMap::new(),
    };
    merger.visit(&mut plan);
    if !merger.replacements.is_empty() {
        replace(&mut plan, &merger.replacements);
    }
    plan
}

fn has_column_select(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::ColumnSelect { .. }) || op.children.iter().any(has_column_select)
}

fn replace(op: &mut LogicalOperator, map: &HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        replace(child, map);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(to) = map.get(binding)
        {
            *binding = *to;
        }
    });
}

/// `IsMergeableAggregateFunction`.
fn is_mergeable_function(aggregate: &BoundAggregate) -> bool {
    let name = aggregate.function.name.as_str();
    if name == "count_star" {
        return aggregate.children.is_empty();
    }
    aggregate.children.len() == 1 && matches!(name, "count" | "sum" | "avg" | "min" | "max")
}

/// `IsSafeOnOtherRows`.
fn is_safe(expr: &BoundExpr) -> bool {
    !expr.is_volatile() && !can_throw(expr)
}

/// `IsMergeableAggregate`.
fn is_mergeable(aggregate: &BoundAggregate) -> bool {
    aggregate.order_by.is_empty()
        && is_mergeable_function(aggregate)
        && aggregate.children.iter().all(is_safe)
        && aggregate.filter.as_ref().is_none_or(is_safe)
}

fn split_conjunction(expr: &BoundExpr, out: &mut Vec<BoundExpr>) {
    if let BoundExprKind::Conjunction { and: true, children } = &expr.kind {
        for child in children {
            split_conjunction(child, out);
        }
        return;
    }
    out.push(expr.clone());
}

/// `IsPlainScan`. The binder gives every scan of a table a `rowid`, which it only reads when an
/// expression refers to it.
fn plain_scan(op: &LogicalOperator) -> Option<Scan> {
    if let K::ParquetScan { table_index, scan } = &op.kind {
        if !scan.filters.is_empty() || scan.projected.is_some() || scan.read.is_some() {
            return None;
        }
        return Some(Scan {
            relation: Relation::Parquet(scan.clone()),
            table_index: *table_index,
            column_ids: (0..scan.output_types().len()).collect(),
            ignore_errors: false,
            read: BTreeSet::new(),
        });
    }
    if let K::CsvScan { table_index, scan } = &op.kind {
        // a scan that stores the rows it rejects writes them on each scan
        if scan.projection.is_some() || scan.rejects.is_some() {
            return None;
        }
        let key = format!(
            "{:?}",
            (
                &scan.files,
                &scan.options,
                &scan.names,
                &scan.types,
                &scan.parsing,
                &scan.hive,
                &scan.force_not_null,
                &scan.manual_types,
                scan.columns_set,
                scan.union_by_name,
                scan.filename,
                &scan.virtual_columns,
            )
        );
        return Some(Scan {
            relation: Relation::File(format!("csv {key}")),
            table_index: *table_index,
            column_ids: scan.projected_columns(),
            ignore_errors: scan.options.ignore_errors,
            read: BTreeSet::new(),
        });
    }
    if let K::JsonScan { table_index, scan } = &op.kind {
        if scan.projection.is_some() {
            return None;
        }
        let key = format!(
            "json {:?}",
            ((
                &scan.files,
                &scan.format,
                &scan.compression,
                scan.records,
                scan.objects,
                &scan.names,
                &scan.types,
            ),
            (
                scan.ignore_errors,
                scan.case_insensitive,
                &scan.date_format,
                &scan.timestamp_format,
                scan.filename,
                &scan.hive,
                &scan.virtual_columns,
            ))
        );
        return Some(Scan {
            relation: Relation::File(key),
            table_index: *table_index,
            column_ids: scan.projected_columns(),
            ignore_errors: scan.ignore_errors,
            read: BTreeSet::new(),
        });
    }
    let K::Get {
        table,
        table_index,
        column_ids,
        filters,
        zone_filters,
        optional_filters,
        projection,
        index_rows,
        sample,
        extracts,
    } = &op.kind
    else {
        return None;
    };
    if extracts.iter().any(Option::is_some) {
        return None;
    }
    if !op.children.is_empty()
        || !filters.is_empty()
        || !zone_filters.is_empty()
        || !optional_filters.is_empty()
        || projection.is_some()
        || index_rows.is_some()
        || sample.is_some()
        || column_ids.contains(&COLUMN_IDENTIFIER_ROW_NUMBER)
    {
        return None;
    }
    Some(Scan {
        relation: Relation::Table(table.clone()),
        table_index: *table_index,
        column_ids: column_ids.clone(),
        ignore_errors: false,
        read: BTreeSet::new(),
    })
}

/// What tells two `read_parquet` calls apart: the function and its arguments as given, not the
/// files they name, and what binding them found.
fn parquet_key(scan: &ParquetScanData) -> String {
    let mut scan = scan.clone();
    scan.column_order = Default::default();
    format!("{scan:?}")
}

/// `IsSameRelation`: the same catalog table, or `read_parquet` with the same arguments.
fn same_relation(a: &Scan, b: &Scan) -> bool {
    match (&a.relation, &b.relation) {
        (Relation::Table(a), Relation::Table(b)) => Arc::ptr_eq(a, b),
        (Relation::Parquet(a), Relation::Parquet(b)) => Arc::ptr_eq(a, b) || parquet_key(a) == parquet_key(b),
        (Relation::File(a), Relation::File(b)) => a == b,
        _ => false,
    }
}

/// `CollectScans`: one scan, or a cross product of scans of different tables.
fn collect_scans(op: &LogicalOperator, scans: &mut Vec<Scan>) -> bool {
    if matches!(op.kind, K::CrossProduct) {
        return op.children.iter().all(|child| collect_scans(child, scans));
    }
    let Some(scan) = plain_scan(op) else {
        return false;
    };
    if scans.iter().any(|other| same_relation(other, &scan)) {
        return false;
    }
    scans.push(scan);
    true
}

/// `CanShareScan`: one scan can replace both. A scan that drops the rows it fails to parse returns
/// a row when every column it reads parses, so such scans are only shared when they read the same.
fn can_share(a: &Scan, b: &Scan) -> bool {
    same_relation(a, b) && (!a.ignore_errors || a.read == b.read)
}

/// `ReadSameTables`.
fn read_same_tables(a: &Branch, b: &Branch) -> bool {
    a.scans.len() == b.scans.len()
        && a.scans
            .iter()
            .all(|scan| b.scans.iter().any(|other| can_share(scan, other)))
}

fn flip(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// `IsSameConjunct`: equal, or the same comparison with its sides swapped.
fn same_conjunct(a: &BoundExpr, b: &BoundExpr) -> bool {
    if expressions_equal(a, b) {
        return true;
    }
    match (&a.kind, &b.kind) {
        (
            BoundExprKind::Comparison {
                op: op_a,
                left: left_a,
                right: right_a,
            },
            BoundExprKind::Comparison {
                op: op_b,
                left: left_b,
                right: right_b,
            },
        ) => *op_a == flip(*op_b) && expressions_equal(left_a, right_b) && expressions_equal(right_a, left_b),
        _ => false,
    }
}

fn contains_conjunct(conjuncts: &[BoundExpr], expr: &BoundExpr) -> bool {
    conjuncts.iter().any(|c| same_conjunct(c, expr))
}

fn remap(expr: &mut BoundExpr, map: &HashMap<ColumnBinding, ColumnBinding>) {
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
            && let Some(to) = map.get(binding)
        {
            *binding = *to;
        }
    });
}

fn remap_aggregate(aggregate: &mut BoundAggregate, map: &HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut aggregate.children {
        remap(child, map);
    }
    if let Some(filter) = &mut aggregate.filter {
        remap(filter, map);
    }
    if let Some(key) = &mut aggregate.distinct_key {
        remap(key, map);
    }
    for (order, _) in &mut aggregate.order_by {
        remap(order, map);
    }
}

fn conjunction(and: bool, mut terms: Vec<BoundExpr>) -> BoundExpr {
    if terms.len() == 1 {
        return terms.pop().expect("one term");
    }
    BoundExpr::new(BoundExprKind::Conjunction { and, children: terms }, LogicalType::BOOLEAN)
}

fn bindings_of(expr: &BoundExpr, out: &mut Vec<ColumnBinding>) {
    expr.clone().walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            out.push(*binding);
        }
    });
}

/// `TargetBinding`: where `target` reads `column`, adding it if it does not yet.
fn target_binding(target: &mut Target, column: usize) -> ColumnBinding {
    let scan = &target.scan;
    if let Some(i) = scan.column_ids.iter().position(|&c| c == column) {
        return ColumnBinding::new(scan.table_index, i);
    }
    if let Some((_, binding)) = target.added.iter().find(|(c, _)| *c == column) {
        return *binding;
    }
    let binding = ColumnBinding::new(scan.table_index, scan.column_ids.len() + target.added.len());
    target.added.push((column, binding));
    binding
}

/// The scan at `table_index` under `op` reads the `added` columns too.
fn add_columns(op: &mut LogicalOperator, table_index: usize, added: &[(usize, ColumnBinding)]) {
    if let K::Get {
        table_index: index,
        column_ids,
        ..
    } = &mut op.kind
        && *index == table_index
    {
        for (column, binding) in added {
            debug_assert_eq!(binding.column_index, column_ids.len());
            column_ids.push(*column);
        }
        return;
    }
    for child in &mut op.children {
        add_columns(child, table_index, added);
    }
}

fn collect_leaves(op: LogicalOperator, leaves: &mut Vec<LogicalOperator>) {
    if matches!(op.kind, K::CrossProduct) {
        for child in op.children {
            collect_leaves(child, leaves);
        }
        return;
    }
    leaves.push(op);
}

impl Merger {
    fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn visit(&mut self, op: &mut LogicalOperator) {
        if matches!(op.kind, K::CrossProduct) {
            self.merge_cross_product(op);
            return;
        }
        for child in &mut op.children {
            self.visit(child);
        }
    }

    /// `IsWorthMerging`.
    fn is_worth_merging(&self, branch: &Branch) -> bool {
        branch.scans.iter().any(|scan| match &scan.relation {
            Relation::Table(table) => self.threshold != 0 && table.row_count() as u64 >= self.threshold,
            Relation::Parquet(_) | Relation::File(_) => true,
        })
    }

    /// `MergeCrossProduct`.
    fn merge_cross_product(&mut self, op: &mut LogicalOperator) {
        let tree = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
        let mut leaves = Vec::new();
        collect_leaves(tree, &mut leaves);
        for leaf in &mut leaves {
            self.visit(leaf);
        }
        let branches: Vec<Branch> = leaves
            .iter()
            .enumerate()
            .filter_map(|(i, leaf)| match_branch(i, leaf))
            .collect();
        // groups of branches that read the same tables, in the order their first branch appears
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..branches.len() {
            match groups
                .iter_mut()
                .find(|group| read_same_tables(&branches[group[0]], &branches[i]))
            {
                Some(group) => group.push(i),
                None => groups.push(vec![i]),
            }
        }
        let mut merged: Vec<Option<LogicalOperator>> = (0..leaves.len()).map(|_| None).collect();
        let mut removed = vec![false; leaves.len()];
        for group in &groups {
            if group.len() < 2 || !self.is_worth_merging(&branches[group[0]]) {
                continue;
            }
            let Some(leaf) = self.merge_branches(&branches, group) else {
                continue;
            };
            merged[branches[group[0]].leaf] = Some(leaf);
            for &k in &group[1..] {
                removed[branches[k].leaf] = true;
            }
        }
        // a cross product's bindings are its children's, so their order changes nothing above it
        let kept: Vec<LogicalOperator> = leaves
            .into_iter()
            .zip(merged)
            .zip(removed)
            .filter(|(_, removed)| !removed)
            .map(|((leaf, merged), _)| merged.unwrap_or(leaf))
            .collect();
        let mut kept = kept.into_iter();
        let mut tree = kept.next().expect("a cross product has leaves");
        for leaf in kept {
            tree = LogicalOperator::new(K::CrossProduct, vec![tree, leaf]);
        }
        *op = tree;
    }

    /// `MergeBranches`.
    fn merge_branches(&mut self, branches: &[Branch], group: &[usize]) -> Option<LogicalOperator> {
        let first = &branches[group[0]];
        let mut targets: Vec<Target> = first
            .scans
            .iter()
            .map(|scan| Target {
                scan: scan.clone(),
                added: Vec::new(),
            })
            .collect();
        let mut remaps: Vec<HashMap<ColumnBinding, ColumnBinding>> = vec![HashMap::new(); group.len()];
        for (k, &b) in group.iter().enumerate().skip(1) {
            for scan in &branches[b].scans {
                let target = targets
                    .iter_mut()
                    .rev()
                    .find(|target| can_share(&target.scan, scan))
                    .expect("the branches read the same tables");
                for (i, &column) in scan.column_ids.iter().enumerate() {
                    remaps[k].insert(ColumnBinding::new(scan.table_index, i), target_binding(target, column));
                }
            }
        }
        // the conjuncts on the target bindings: the ones every branch has, and each one's own
        let conjuncts: Vec<Vec<BoundExpr>> = group
            .iter()
            .enumerate()
            .map(|(k, &b)| {
                branches[b]
                    .conjuncts
                    .iter()
                    .map(|c| {
                        let mut c = c.clone();
                        remap(&mut c, &remaps[k]);
                        c
                    })
                    .collect()
            })
            .collect();
        let mut shared: Vec<BoundExpr> = Vec::new();
        for conjunct in &conjuncts[0] {
            if !contains_conjunct(&shared, conjunct) && conjuncts[1..].iter().all(|cs| contains_conjunct(cs, conjunct))
            {
                shared.push(conjunct.clone());
            }
        }
        let own: Vec<Vec<BoundExpr>> = conjuncts
            .into_iter()
            .map(|cs| cs.into_iter().filter(|c| !contains_conjunct(&shared, c)).collect())
            .collect();
        // over several tables, every own conjunct reads one and the same table
        let mut own_table = None;
        for conjunct in own.iter().flatten() {
            let mut bindings = Vec::new();
            bindings_of(conjunct, &mut bindings);
            for binding in bindings {
                match own_table {
                    None => own_table = Some(binding.table_index),
                    Some(table) if table != binding.table_index => return None,
                    _ => {}
                }
            }
        }
        // the group merges
        let mut relation = first.relation.clone();
        for target in &targets {
            add_columns(&mut relation, target.scan.table_index, &target.added);
        }
        let mut every_branch_restricted = true;
        let predicates: Vec<Option<BoundExpr>> = own
            .into_iter()
            .map(|terms| {
                if terms.is_empty() {
                    every_branch_restricted = false;
                    None
                } else {
                    Some(conjunction(true, terms))
                }
            })
            .collect();
        let aggregate_index = self.take_index();
        let mut aggregates = Vec::new();
        let mut offsets = Vec::new();
        for (k, &b) in group.iter().enumerate() {
            offsets.push(aggregates.len());
            for aggregate in &branches[b].aggregates {
                let mut copy = aggregate.clone();
                remap_aggregate(&mut copy, &remaps[k]);
                if let Some(predicate) = &predicates[k] {
                    copy.filter = Some(match copy.filter.take() {
                        Some(filter) => conjunction(true, vec![predicate.clone(), filter]),
                        None => predicate.clone(),
                    });
                }
                aggregates.push(copy);
            }
        }
        let projection_index = self.take_index();
        let mut columns: Vec<BoundExpr> = Vec::new();
        for (k, &b) in group.iter().enumerate() {
            let branch = &branches[b];
            let to_merged: HashMap<ColumnBinding, ColumnBinding> = (0..branch.aggregates.len())
                .map(|j| {
                    (
                        ColumnBinding::new(branch.aggregate_index, j),
                        ColumnBinding::new(aggregate_index, offsets[k] + j),
                    )
                })
                .collect();
            match &branch.projection {
                Some((table_index, expressions)) => {
                    for (i, expr) in expressions.iter().enumerate() {
                        let mut copy = expr.clone();
                        remap(&mut copy, &to_merged);
                        self.replacements.insert(
                            ColumnBinding::new(*table_index, i),
                            ColumnBinding::new(projection_index, columns.len()),
                        );
                        columns.push(copy);
                    }
                }
                None => {
                    for (j, aggregate) in branch.aggregates.iter().enumerate() {
                        self.replacements.insert(
                            ColumnBinding::new(branch.aggregate_index, j),
                            ColumnBinding::new(projection_index, columns.len()),
                        );
                        columns.push(BoundExpr::column_ref(
                            ColumnBinding::new(aggregate_index, offsets[k] + j),
                            aggregate.return_type.clone(),
                            "",
                        ));
                    }
                }
            }
        }
        let mut filters = shared;
        if every_branch_restricted {
            // branches with the same predicate contribute one alternative
            let mut alternatives: Vec<BoundExpr> = Vec::new();
            for predicate in predicates.into_iter().flatten() {
                if !contains_conjunct(&alternatives, &predicate) {
                    alternatives.push(predicate);
                }
            }
            // the OR is a new expression, which goes through the rewrite rules (predicate factoring)
            let mut or = conjunction(false, alternatives);
            rewrite_expression(&mut or, true);
            split_conjunction(&or, &mut filters);
        }
        if !filters.is_empty() {
            relation = LogicalOperator::new(K::Filter { expressions: filters }, vec![relation]);
        }
        let aggregate = LogicalOperator::new(
            K::Aggregate {
                group_index: self.take_index(),
                aggregate_index,
                groupings_index: self.take_index(),
                groups: Vec::new(),
                aggregates,
                grouping_sets: first.grouping_sets.clone(),
                grouping_functions: Vec::new(),
                perfect_hash: false,
                domain_cap: None,
            },
            vec![relation],
        );
        Some(LogicalOperator::new(
            K::Projection {
                table_index: projection_index,
                expressions: columns,
            },
            vec![aggregate],
        ))
    }
}

/// `TryMatchBranch`.
fn match_branch(leaf: usize, op: &LogicalOperator) -> Option<Branch> {
    let mut op = op;
    let mut projection = None;
    if let K::Projection {
        table_index,
        expressions,
    } = &op.kind
    {
        projection = Some((*table_index, expressions.clone()));
        op = &op.children[0];
    }
    let K::Aggregate {
        aggregate_index,
        groups,
        aggregates,
        grouping_sets,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return None;
    };
    // an aggregate without groups or grouping sets returns exactly one row
    if !groups.is_empty()
        || !grouping_functions.is_empty()
        || grouping_sets.len() > 1
        || grouping_sets.first().is_some_and(|set| !set.is_empty())
        || aggregates.is_empty()
        || !aggregates.iter().all(is_mergeable)
    {
        return None;
    }
    let mut conjuncts = Vec::new();
    let mut relation = &op.children[0];
    while let K::Filter { expressions } = &relation.kind {
        for expr in expressions {
            split_conjunction(expr, &mut conjuncts);
        }
        relation = &relation.children[0];
    }
    if !conjuncts.iter().all(is_safe) {
        return None;
    }
    let mut scans = Vec::new();
    if !collect_scans(relation, &mut scans) {
        return None;
    }
    // `rowid` is a virtual column
    let row_ids: Vec<ColumnBinding> = scans
        .iter()
        .filter(|scan| matches!(scan.relation, Relation::Table(_)))
        .flat_map(|scan| {
            scan.column_ids
                .iter()
                .enumerate()
                .filter(|(_, c)| **c == COLUMN_IDENTIFIER_ROW_ID)
                .map(|(i, _)| ColumnBinding::new(scan.table_index, i))
        })
        .collect();
    let mut read = Vec::new();
    for conjunct in &conjuncts {
        bindings_of(conjunct, &mut read);
    }
    for aggregate in aggregates {
        for child in aggregate.children.iter().chain(&aggregate.filter).chain(&aggregate.distinct_key) {
            bindings_of(child, &mut read);
        }
    }
    if read.iter().any(|binding| row_ids.contains(binding)) {
        return None;
    }
    for scan in &mut scans {
        scan.read = read
            .iter()
            .filter(|binding| binding.table_index == scan.table_index)
            .map(|binding| binding.column_index)
            .collect();
    }
    Some(Branch {
        leaf,
        projection,
        aggregate_index: *aggregate_index,
        aggregates: aggregates.clone(),
        grouping_sets: grouping_sets.clone(),
        relation: relation.clone(),
        scans,
        conjuncts,
    })
}
