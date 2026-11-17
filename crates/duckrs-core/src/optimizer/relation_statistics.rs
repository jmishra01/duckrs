// Ported from duckdb/src/optimizer/relation_statistics/{relation_statistics,relation_statistics_helper,
// relation_statistics_operator,relation_statistics_scan}.cpp @ rust-port-baseline
//
//! `RelationStats`: what the join order optimizer and the cardinality estimates know of the rows of
//! a plan: how many there are, and for each column how many values it can have (`total_domain`, the
//! denominator of equality joins) and how many of them are still there (`current_domain`), with the
//! evidence that lets SEMI and ANTI joins use the latter.

use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{HashMap, HashSet};

/// `RelationStatisticsHelper::DEFAULT_SELECTIVITY`.
pub(super) const DEFAULT_SELECTIVITY: f64 = 0.2;

/// `MAX_BASE_CARDINALITY_TO_DOMAIN_RATIO`.
const MAX_BASE_CARDINALITY_TO_DOMAIN_RATIO: f64 = 8.0;

/// `DistinctCountSource`: where a count comes from, least to most trustworthy. `EXACT` counts as
/// `HLL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CountSource {
    Cardinality,
    MinMax,
    Hll,
}

/// `DistinctCount`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DistinctCount {
    pub count: f64,
    pub source: CountSource,
}

impl DistinctCount {
    pub fn new(count: f64, source: CountSource) -> Self {
        Self { count, source }
    }

    pub fn cardinality(count: f64) -> Self {
        Self::new(count, CountSource::Cardinality)
    }

    /// `HasReliableDistinctCount`
    pub fn is_reliable(&self) -> bool {
        self.source == CountSource::Hll
    }
}

/// `SemiAntiJoinDomainEvidence`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct DomainEvidence {
    filter_bound: Option<f64>,
    unique: bool,
}

impl DomainEvidence {
    /// `TightenFilterDomainBound`
    pub fn tighten(&mut self, bound: f64) {
        if self.filter_bound.is_none_or(|b| bound < b) {
            self.filter_bound = Some(bound);
        }
    }

    /// `MarkUnique`
    pub fn mark_unique(&mut self) {
        self.unique = true;
    }

    /// `GetSupportedDomainSize`
    pub fn supported(&self, current: f64) -> Option<f64> {
        let unique = self.unique.then_some(current);
        match (unique, self.filter_bound) {
            (Some(u), Some(b)) => Some(u.min(b)),
            (u, b) => u.or(b),
        }
    }
}

/// `RelationColumnStats`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ColumnStats {
    pub total: DistinctCount,
    pub current: DistinctCount,
    pub evidence: DomainEvidence,
}

impl ColumnStats {
    pub fn new(domain: DistinctCount) -> Self {
        Self::with(domain, domain)
    }

    pub fn with(total: DistinctCount, current: DistinctCount) -> Self {
        Self {
            total,
            current,
            evidence: DomainEvidence::default(),
        }
    }

    /// Without the SEMI/ANTI evidence, as most operators pass a column on.
    pub fn plain(&self) -> Self {
        Self::with(self.total, self.current)
    }
}

/// `RelationStats`. A column the map does not have is one of `cardinality` values, as C++ has
/// for the columns it has no statistics of.
#[derive(Debug, Clone)]
pub(super) struct RelationStats {
    pub cardinality: f64,
    pub columns: HashMap<ColumnBinding, ColumnStats>,
    /// `row_retention`: the fraction of the rows of the last source of statistics still there
    pub retention: f64,
}

impl Default for RelationStats {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl RelationStats {
    pub fn new(cardinality: f64) -> Self {
        Self {
            cardinality,
            columns: HashMap::new(),
            retention: 1.0,
        }
    }

    /// Each of `bindings` with as many values as there are rows.
    pub fn uniform(bindings: impl IntoIterator<Item = ColumnBinding>, cardinality: f64) -> Self {
        let mut stats = Self::new(cardinality);
        for binding in bindings {
            stats
                .columns
                .insert(binding, ColumnStats::new(DistinctCount::cardinality(cardinality)));
        }
        stats
    }

    /// The statistics of a column, the default ones if there are none.
    pub fn column(&self, binding: &ColumnBinding) -> ColumnStats {
        self.columns
            .get(binding)
            .cloned()
            .unwrap_or_else(|| ColumnStats::new(DistinctCount::cardinality(self.cardinality)))
    }

    /// `current_domain`
    pub fn current(&self, binding: &ColumnBinding) -> f64 {
        self.column(binding).current.count
    }

    /// `CapCurrentDomainsToCardinality`
    pub fn cap_current_domains(&mut self) {
        let cardinality = self.cardinality;
        for column in self.columns.values_mut() {
            column.current.count = column.current.count.min(cardinality);
        }
    }

    /// `RelationStatisticsHelper::ApplyCardinality`.
    pub fn apply_cardinality(&mut self, output: f64) {
        let input = self.cardinality;
        for column in self.columns.values_mut() {
            column.current = estimate_current_domain(column.current, input, output);
            column.current.count = column.current.count.min(column.total.count);
        }
        if input > 0.0 && output < input {
            self.retention *= output / input;
        }
        self.cardinality = output;
    }

    /// The columns of `bindings` of these statistics (`ProjectOutputStats`).
    pub fn project(&self, bindings: &[ColumnBinding]) -> Self {
        let mut result = Self {
            cardinality: self.cardinality,
            columns: HashMap::new(),
            retention: self.retention,
        };
        for binding in bindings {
            result.columns.insert(*binding, self.column(binding));
        }
        result
    }
}

/// `RelationStatisticsHelper::SelectTotalDomain`: the largest reliable count, else the largest of
/// minimum and maximum, else the smallest fallback.
pub(super) fn select_total_domain(domains: &[DistinctCount]) -> Option<DistinctCount> {
    let best = |source: CountSource, largest: bool| {
        domains.iter().filter(|d| d.source == source).copied().reduce(|a, b| {
            if (b.count > a.count) == largest && b.count != a.count {
                b
            } else {
                a
            }
        })
    };
    best(CountSource::Hll, true)
        .or_else(|| best(CountSource::MinMax, true))
        .or_else(|| best(CountSource::Cardinality, false))
}

/// `RelationStatisticsHelper::EstimateCurrentDomain`: how many of `current` values the rows keep
/// when `output` of `input` rows remain, each value on as many rows.
pub(super) fn estimate_current_domain(current: DistinctCount, input: f64, output: f64) -> DistinctCount {
    let count = current.count.min(input);
    if count <= 0.0 || output <= 0.0 || input <= 0.0 {
        return DistinctCount::cardinality(0.0);
    }
    if output >= input {
        return DistinctCount::new(count, current.source);
    }
    let row_survival = output / input;
    let multiplicity = input / count;
    let retained_fraction = -(multiplicity * (-row_survival).ln_1p()).exp_m1();
    let estimate = (count * retained_fraction).round();
    let result = estimate.max(0.0).floor().min(output).min(count);
    DistinctCount::cardinality(result.max(1.0))
}

/// `RelationStatisticsHelper::EstimateDistinctCardinality`: the distinct combinations of columns
/// with these counts among `input` rows.
pub(super) fn estimate_distinct_cardinality(counts: &[DistinctCount], input: f64) -> f64 {
    match counts {
        [] => (input / 2.0).floor(),
        [only] => only.count.min(input),
        _ => {
            let mut product: f64 = counts.iter().map(|c| c.count.max(1.0)).product();
            product *= 0.95f64.powi(counts.len() as i32 - 1);
            let multiplier = 1.0 - (-input / product).exp();
            let estimate = if multiplier == 0.0 { input } else { product * multiplier };
            let result = estimate.max(0.0).floor().min(input);
            if input > 0.0 { result.max(1.0) } else { 0.0 }
        }
    }
}

/// `MatchesTargetColumn`.
fn matches_target(expr: &BoundExpr, target: ColumnBinding) -> bool {
    matches!(expr.kind, BoundExprKind::ColumnRef { binding, depth: 0 } if binding == target)
}

/// `RelationStatisticsHelper::EstimateDirectFilterDomain`: how many values of `target` a filter
/// can let through, from its equalities and IN lists.
pub(super) fn direct_filter_domain(expr: &BoundExpr, target: ColumnBinding, maximum: f64) -> Option<f64> {
    match &expr.kind {
        BoundExprKind::Comparison { op, left, right } => {
            if *op != ComparisonOp::Equal {
                return None;
            }
            let constant = if matches_target(left, target) {
                right
            } else if matches_target(right, target) {
                left
            } else {
                return None;
            };
            match &constant.kind {
                BoundExprKind::Constant(value) if !value.is_null() => Some(maximum.min(1.0)),
                _ => None,
            }
        }
        BoundExprKind::Operator {
            op: BoundOperator::In,
            children,
        } => {
            if children.is_empty() || !matches_target(&children[0], target) {
                return None;
            }
            let mut constants: Vec<&Value> = Vec::new();
            for child in &children[1..] {
                let BoundExprKind::Constant(value) = &child.kind else {
                    return None;
                };
                if !value.is_null() && !constants.contains(&value) {
                    constants.push(value);
                }
            }
            Some(maximum.min(constants.len() as f64))
        }
        BoundExprKind::Conjunction { and: true, children } => children
            .iter()
            .filter_map(|child| direct_filter_domain(child, target, maximum))
            .reduce(f64::min),
        BoundExprKind::Conjunction { and: false, children } => {
            let mut result = 0.0;
            for child in children {
                let bound = direct_filter_domain(child, target, maximum)?;
                if bound >= maximum - result {
                    return Some(maximum);
                }
                result += bound;
            }
            Some(result)
        }
        _ => None,
    }
}

/// `ApplyFilterDomainCap` of what `expr` says of `target`.
pub(super) fn apply_direct_filter_domain(column: &mut ColumnStats, expr: &BoundExpr, target: ColumnBinding) {
    let Some(cap) = direct_filter_domain(expr, target, column.total.count) else {
        return;
    };
    column.current = DistinctCount::cardinality(column.current.count.min(cap));
    column.evidence.tighten(cap);
}

/// `JoinOrderUtil::GetEquivalenceBinding`: the column an expression is, also through a cast that
/// keeps its values apart.
pub(super) fn equivalence_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        BoundExprKind::Cast { child, try_cast: false }
            if super::rewriter::cast_is_invertible(&child.return_type, &expr.return_type) =>
        {
            equivalence_binding(child)
        }
        _ => None,
    }
}

/// `SemiAntiJoinDomain`.
#[derive(Debug, Clone, Copy)]
pub(super) struct SemiAntiDomain {
    pub total_domain: f64,
    pub rhs_current_domain: f64,
    pub preserved: ColumnBinding,
    pub rhs: ColumnBinding,
}

/// A condition of a SEMI or ANTI join: its sides and how they compare.
pub(super) struct SemiAntiCondition<'a> {
    pub left: &'a BoundExpr,
    pub right: &'a BoundExpr,
    pub op: ComparisonOp,
}

/// `ExtractSemiAntiJoinDomains`: the domains of the key equalities, and whether there are other
/// conditions (`has_residual`).
pub(super) fn semi_anti_domains(
    conditions: &[SemiAntiCondition],
    preserved: &RelationStats,
    rhs: &RelationStats,
    preserve_right: bool,
) -> (Vec<SemiAntiDomain>, bool) {
    let mut domains: Vec<SemiAntiDomain> = Vec::new();
    let mut residual = false;
    let column = |expr: &BoundExpr, stats: &RelationStats| {
        let binding = equivalence_binding(expr)?;
        stats.columns.get(&binding).map(|c| (binding, c.clone()))
    };
    for condition in conditions {
        if !matches!(condition.op, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom) {
            residual = true;
            continue;
        }
        let (lhs_expr, rhs_expr) = if preserve_right {
            (condition.right, condition.left)
        } else {
            (condition.left, condition.right)
        };
        let mut sides = column(lhs_expr, preserved).zip(column(rhs_expr, rhs));
        if sides.is_none() {
            sides = column(rhs_expr, preserved).zip(column(lhs_expr, rhs));
        }
        let Some(((preserved_binding, preserved_column), (rhs_binding, rhs_column))) = sides else {
            residual = true;
            continue;
        };
        if domains
            .iter()
            .any(|d| d.preserved == preserved_binding && d.rhs == rhs_binding)
        {
            continue;
        }
        let Some(rhs_current_domain) = rhs_column.evidence.supported(rhs_column.current.count) else {
            residual = true;
            continue;
        };
        let total = select_total_domain(&[preserved_column.total, rhs_column.total]);
        let Some(total) = total.filter(|t| t.count > 0.0) else {
            residual = true;
            continue;
        };
        domains.push(SemiAntiDomain {
            total_domain: total.count,
            rhs_current_domain,
            preserved: preserved_binding,
            rhs: rhs_binding,
        });
    }
    (domains, residual)
}

fn is_anti(join_type: JoinType) -> bool {
    matches!(join_type, JoinType::Anti | JoinType::RightAnti)
}

/// `EstimateSemiAntiJoinFallbackCardinality`.
pub(super) fn semi_anti_fallback(preserved: f64, rhs: f64, join_type: JoinType) -> f64 {
    if rhs == 0.0 {
        return if is_anti(join_type) { preserved.max(0.0) } else { 0.0 };
    }
    preserved.min((preserved * DEFAULT_SELECTIVITY).max(1.0)).max(0.0)
}

/// `RoundSemiAntiCardinality`.
fn round_semi_anti(preserved: f64, selectivity: f64) -> f64 {
    if preserved <= 0.0 {
        return 0.0;
    }
    let mut estimate = (preserved * selectivity.clamp(0.0, 1.0)).round();
    if estimate <= 0.0 {
        estimate = 1.0;
    }
    estimate.min(preserved)
}

/// `EstimateSemiAntiJoinCardinality`: the rows a SEMI or ANTI join keeps, and whether the domains
/// told.
pub(super) fn semi_anti_cardinality(
    preserved: f64,
    rhs: f64,
    rhs_retention: f64,
    join_type: JoinType,
    domains: &[SemiAntiDomain],
    residual: bool,
) -> (f64, bool) {
    let fallback = semi_anti_fallback(preserved, rhs, join_type);
    if preserved <= 0.0 || rhs == 0.0 || residual || domains.is_empty() {
        return (fallback, false);
    }
    let mut match_fraction = 1.0;
    let mut strongest_fraction = 1.0;
    let mut strongest_total = 0.0;
    let mut maximum_total: f64 = 0.0;
    let mut preserved_bindings = HashSet::new();
    let mut rhs_bindings = HashSet::new();
    for domain in domains {
        if domain.total_domain == 0.0 {
            return (fallback, false);
        }
        let fraction = (domain.rhs_current_domain / domain.total_domain).min(1.0);
        match_fraction *= fraction;
        if fraction < strongest_fraction {
            strongest_fraction = fraction;
            strongest_total = domain.total_domain;
        }
        maximum_total = maximum_total.max(domain.total_domain);
        preserved_bindings.insert(domain.preserved);
        rhs_bindings.insert(domain.rhs);
    }
    if domains.len() >= 2 && preserved_bindings.len() >= 2 && rhs_bindings.len() >= 2 && maximum_total > 0.0 {
        let retention = rhs_retention.clamp(0.0, 1.0);
        let rhs_base = if retention > 0.0 { rhs / retention } else { rhs };
        if rhs_base <= maximum_total * MAX_BASE_CARDINALITY_TO_DOMAIN_RATIO
            && (strongest_total == 0.0 || strongest_total <= rhs_base)
        {
            match_fraction = match_fraction.max(strongest_fraction.min(retention));
        }
    }
    let match_fraction = match_fraction.clamp(0.0, 1.0);
    let selectivity = if is_anti(join_type) {
        1.0 - match_fraction
    } else {
        match_fraction
    };
    (round_semi_anti(preserved, selectivity), true)
}

/// `RelationStatisticsHelper::ApplySemiAntiJoinStats`: the statistics of a SEMI or ANTI join,
/// those of its preserved side with the rows it keeps, and for a SEMI join its keys narrowed to
/// the values of the other side.
pub(super) fn semi_anti_join_stats(
    conditions: &[SemiAntiCondition],
    join_type: JoinType,
    left: &RelationStats,
    right: &RelationStats,
) -> RelationStats {
    let preserve_right = matches!(join_type, JoinType::RightSemi | JoinType::RightAnti);
    let (preserved, rhs) = if preserve_right { (right, left) } else { (left, right) };
    // `ProjectChildStats` does not keep the evidence
    let mut result = RelationStats::new(preserved.cardinality);
    for (binding, column) in &preserved.columns {
        result.columns.insert(*binding, column.plain());
    }
    result.cap_current_domains();
    let (domains, residual) = semi_anti_domains(conditions, preserved, rhs, preserve_right);
    let (estimate, _) = semi_anti_cardinality(
        preserved.cardinality,
        rhs.cardinality,
        rhs.retention,
        join_type,
        &domains,
        residual,
    );
    result.retention = preserved.retention;
    let mut cardinality = preserved.cardinality.min(estimate.max(0.0).floor());
    if rhs.cardinality == 0.0 && is_anti(join_type) {
        cardinality = preserved.cardinality;
    }
    result.apply_cardinality(cardinality);
    if !is_anti(join_type) {
        for domain in &domains {
            if let Some(column) = result.columns.get_mut(&domain.preserved) {
                column.current = DistinctCount::cardinality(column.current.count.min(domain.rhs_current_domain));
            }
        }
    }
    result
}
