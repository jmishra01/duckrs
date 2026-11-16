// Ported from duckdb/src/optimizer/projection_placement.cpp and expression_heuristics.cpp @ rust-port-baseline
//
//! `ProjectionPlacementOptimizer`: where a computed expression of a projection runs, around inner
//! joins, by cost. An expression above a join that reads one side only moves below it when that
//! side has far fewer rows (or its payload shrinks enough), and an expression computed below joins
//! moves above them when they cut the rows down. The cost of a position is the rows the expression
//! runs on times the bytes it reads and writes, and the rows its inputs and result travel through
//! the joins times their widths; widths of strings come from the statistics of their bindings.
//!
//! C++ keeps the operators to change by reference; here they are found by their paths from the
//! root. duckrs operators pass on the columns of their children by binding, so C++'s projection
//! maps (`ExposeBinding`, `RemapParentProjectionMap`) have no counterpart.

use super::common_subplan::{max_table_index, node, node_mut, visit_bindings};
use super::rewriter::can_throw;
use super::statistics::StatisticsMap;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_parser::ast::JoinType;
use std::collections::{HashMap, HashSet};

type Path = Vec<usize>;

const MINIMUM_COST_RATIO: f64 = 0.75;

/// `ProjectionPlacementOptimizer::Optimize`.
pub fn place(plan: &mut LogicalOperator, statistics: &mut StatisticsMap) {
    let mut placer = Placer {
        statistics,
        next_table_index: max_table_index(plan) + 1,
        cardinalities: HashMap::new(),
    };
    while placer.lift_first_profitable(plan) {}
    // `OptimizeOperator`: the projections children first; what a placement adds below a
    // projection is not looked at again
    let mut projections = Vec::new();
    projections_in_post_order(plan, &mut Vec::new(), &mut projections);
    for path in projections {
        placer.optimize_projection(plan, &path);
    }
}

fn projections_in_post_order(op: &LogicalOperator, path: &mut Path, result: &mut Vec<Path>) {
    for (at, child) in op.children.iter().enumerate() {
        path.push(at);
        projections_in_post_order(child, path, result);
        path.pop();
    }
    if matches!(op.kind, K::Projection { .. }) {
        result.push(path.clone());
    }
}

/// `ExpressionHeuristics::Cost`.
pub fn expression_cost(expr: &BoundExpr) -> u64 {
    let type_cost = |ty: &LogicalType, multiplier: u64| match ty.physical_type() {
        PhysicalType::Varchar => 5 * multiplier,
        PhysicalType::Float | PhysicalType::Double => 2 * multiplier,
        _ => multiplier,
    };
    match &expr.kind {
        BoundExprKind::Case { checks, else_expr } => {
            checks
                .iter()
                .map(|(when, then)| expression_cost(when) + expression_cost(then))
                .sum::<u64>()
                + expression_cost(else_expr)
        }
        BoundExprKind::Conjunction { children, .. } => 5 + children.iter().map(expression_cost).sum::<u64>(),
        BoundExprKind::Between { input, lower, upper } => {
            expression_cost(input) + expression_cost(lower) + expression_cost(upper) + 10
        }
        BoundExprKind::Comparison { left, right, .. } => expression_cost(left) + 5 + expression_cost(right),
        BoundExprKind::Cast { child, .. } => {
            let (source, target) = (&child.return_type, &expr.return_type);
            let cast_cost = if source == target {
                0
            } else if [source, target].iter().any(|ty| {
                matches!(
                    ty.id(),
                    duckrs_common::types::LogicalTypeId::Varchar | duckrs_common::types::LogicalTypeId::Blob
                )
            }) {
                200
            } else {
                5
            };
            expression_cost(child) + cast_cost
        }
        BoundExprKind::Function { function, children, .. } => {
            let own = match function.name.as_str() {
                "+" | "-" | "&" | "#" | ">>" | "<<" | "abs" => 5,
                "*" | "%" => 10,
                "/" => 15,
                "date_part" | "year" => 20,
                "round" => 100,
                "~~" | "!~~" | "regexp_matches" | "||" => 200,
                _ => 1000,
            };
            children.iter().map(expression_cost).sum::<u64>() + own
        }
        BoundExprKind::Operator { op, children } => {
            let sum = children.iter().map(expression_cost).sum::<u64>();
            sum + match op {
                BoundOperator::IsNull | BoundOperator::IsNotNull => 5,
                BoundOperator::Not => 10,
                _ => 1000,
            }
        }
        BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => type_cost(&expr.return_type, 8),
        BoundExprKind::Constant(_) => type_cost(&expr.return_type, 1),
        _ => 1000,
    }
}

/// Calls `f` on `expr` and each expression below it.
fn walk<'a>(expr: &'a BoundExpr, f: &mut impl FnMut(&'a BoundExpr)) {
    f(expr);
    for child in expr.children() {
        walk(child, f);
    }
}

fn is_column_ref(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::ColumnRef { .. })
}

/// The columns `expr` reads, once each in the order it reads them; `None` if it reads one of an
/// outer query.
fn read_bindings(expr: &BoundExpr) -> Option<Vec<(ColumnBinding, LogicalType)>> {
    let mut result: Vec<(ColumnBinding, LogicalType)> = Vec::new();
    let mut valid = true;
    walk(expr, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth } = &e.kind {
            if *depth != 0 {
                valid = false;
            } else if !result.iter().any(|(b, _)| b == binding) {
                result.push((*binding, e.return_type.clone()));
            }
        }
    });
    valid.then_some(result)
}

fn references_any(expr: &BoundExpr, bindings: &HashSet<ColumnBinding>) -> bool {
    let mut result = false;
    walk(expr, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            result |= bindings.contains(binding);
        }
    });
    result
}

/// `OperatorReferencesAnyBinding`: whether the expressions of `op` itself read any of `bindings`.
fn operator_references_any(op: &LogicalOperator, bindings: &HashSet<ColumnBinding>) -> bool {
    let mut kind = op.kind.clone();
    let mut result = false;
    visit_bindings(&mut kind, &mut |binding, depth| {
        result |= depth == 0 && bindings.contains(binding);
    });
    result
}

/// `RetainedBindings`.
fn retained(op: &LogicalOperator, bindings: &HashSet<ColumnBinding>) -> HashSet<ColumnBinding> {
    op.column_bindings().into_iter().filter(|b| bindings.contains(b)).collect()
}

/// `BindingsBelongToChild`: `expr` reads columns, all of them columns of `op`.
fn bindings_belong_to(expr: &BoundExpr, op: &LogicalOperator) -> bool {
    let bindings = op.column_bindings();
    let (mut found, mut valid) = (false, true);
    walk(expr, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth } = &e.kind {
            found = true;
            valid &= *depth == 0 && bindings.contains(binding);
        }
    });
    found && valid
}

/// `IsEligibleJoin`: an inner comparison or arbitrary join.
fn is_eligible_join(op: &LogicalOperator) -> bool {
    matches!(
        op.kind,
        K::Join {
            join_type: JoinType::Inner,
            ..
        }
    )
}

/// `RewriteThroughProjection`: `expr` reading the columns the projection's column references read.
fn rewrite_through_projection(expr: &mut BoundExpr, table_index: usize, expressions: &[BoundExpr]) -> bool {
    let mut valid = true;
    expr.walk_mut(&mut |e| {
        let ty = e.return_type.clone();
        if let BoundExprKind::ColumnRef { binding, depth } = &mut e.kind {
            if !valid || *depth != 0 || binding.table_index != table_index {
                valid = false;
                return;
            }
            match expressions.get(binding.column_index).map(|s| (&s.kind, &s.return_type)) {
                Some((BoundExprKind::ColumnRef { binding: source, depth: 0 }, source_type)) if *source_type == ty => {
                    *binding = *source;
                }
                _ => valid = false,
            }
        }
    });
    valid
}

/// `CountReadingExpressions`.
fn count_reading_expressions(expressions: &[BoundExpr]) -> HashMap<ColumnBinding, usize> {
    let mut result = HashMap::new();
    for expr in expressions {
        let mut read = HashSet::new();
        walk(expr, &mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind
                && read.insert(*binding)
            {
                *result.entry(*binding).or_insert(0) += 1;
            }
        });
    }
    result
}

/// `GetExclusiveInputWidth`.
fn exclusive_input_width(reading: &HashMap<ColumnBinding, usize>, widths: &[(ColumnBinding, LogicalType, u64)]) -> u64 {
    widths
        .iter()
        .filter(|(binding, ..)| reading.get(binding).is_none_or(|&n| n <= 1))
        .map(|(.., width)| width)
        .sum()
}

/// Replaces `map`'s bindings in the expressions of `op` and below, except in the projection whose
/// table index is `stop` (`ColumnBindingReplacer` with a `stop_operator`).
fn replace_bindings(op: &mut LogicalOperator, stop: Option<usize>, map: &HashMap<ColumnBinding, ColumnBinding>) {
    if map.is_empty() {
        return;
    }
    if let K::Projection { table_index, .. } = &op.kind
        && Some(*table_index) == stop
    {
        return;
    }
    for child in &mut op.children {
        replace_bindings(child, stop, map);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}

/// The replacements of only the expressions of `op` itself.
fn replace_own_bindings(op: &mut LogicalOperator, map: &HashMap<ColumnBinding, ColumnBinding>) {
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}

/// `ResolveBinding`: where a binding ended up through the projections rebuilt one after the other.
fn resolve(binding: ColumnBinding, replacements: &[(ColumnBinding, ColumnBinding)]) -> ColumnBinding {
    let mut result = binding;
    for _ in 0..=replacements.len() {
        match replacements.iter().find(|(old, _)| *old == result) {
            Some((_, new)) => result = *new,
            None => return result,
        }
    }
    result
}

/// `ExpressionPlacement`: an expression of a projection, to compute below the joins of `steps`.
struct Placement {
    expression_index: usize,
    /// the operators between the projection and the target, with the child taken, top first
    steps: Vec<(Path, usize)>,
    /// the expression, reading the columns of the target
    expression: BoundExpr,
}

impl Placement {
    fn target(&self) -> Path {
        let (path, child) = self.steps.last().expect("a step");
        let mut target = path.clone();
        target.push(*child);
        target
    }
}

/// `LiftPathStep`.
struct LiftStep {
    path: Path,
    result_bindings: HashSet<ColumnBinding>,
}

/// `LiftPlacement`.
struct LiftPlacement {
    source: Path,
    expression_index: usize,
    source_binding: ColumnBinding,
    dependencies: Vec<(ColumnBinding, LogicalType)>,
    steps: Vec<LiftStep>,
    result_bindings: HashSet<ColumnBinding>,
    target_bindings: Vec<ColumnBinding>,
    target_types: Vec<LogicalType>,
}

/// `LiftDependency`.
struct Dependency {
    original: ColumnBinding,
    current: ColumnBinding,
    ty: LogicalType,
}

struct Placer<'a> {
    statistics: &'a mut StatisticsMap,
    next_table_index: usize,
    /// the estimates of the operators, by address, until the plan changes
    cardinalities: HashMap<usize, f64>,
}

impl Placer<'_> {
    fn generate_table_index(&mut self) -> usize {
        self.next_table_index += 1;
        self.next_table_index - 1
    }

    /// `estimated_cardinality`, as the join order optimizer works it out.
    fn cardinality(&mut self, op: &LogicalOperator) -> f64 {
        let key = op as *const LogicalOperator as usize;
        *self
            .cardinalities
            .entry(key)
            .or_insert_with(|| super::join_order::estimated_cardinality(op) as f64)
    }

    /// `GetBindingWidth`: the bytes of a value; of a string only when its statistics bound it.
    fn binding_width(&mut self, binding: ColumnBinding, ty: &LogicalType) -> Option<u64> {
        let physical = ty.physical_type();
        if physical.is_constant_size() {
            return Some(physical.size() as u64);
        }
        if physical != PhysicalType::Varchar {
            return None;
        }
        let max = self.statistics.get(binding).strings?.max_length?;
        Some(physical.size() as u64 + max)
    }

    /// `GetExpressionWidth`: the bytes of the columns an expression reads, with each one's.
    fn expression_width(&mut self, expr: &BoundExpr) -> Option<(u64, Vec<(ColumnBinding, LogicalType, u64)>)> {
        let bindings = read_bindings(expr)?;
        if bindings.is_empty() {
            return None;
        }
        let mut total = 0;
        let mut widths = Vec::with_capacity(bindings.len());
        for (binding, ty) in bindings {
            let width = self.binding_width(binding, &ty)?;
            total += width;
            widths.push((binding, ty, width));
        }
        Some((total, widths))
    }

    /// The operators of `plan` changed: the estimates by address are no longer of them.
    fn changed(&mut self) {
        self.cardinalities.clear();
    }

    // ---- placing below joins

    /// The `OptimizeOperator` loop of one projection.
    fn optimize_projection(&mut self, plan: &mut LogicalOperator, projection: &Path) {
        loop {
            let K::Projection { expressions, .. } = &node(plan, projection).kind else {
                return;
            };
            let reading = count_reading_expressions(expressions);
            let count = expressions.len();
            let mut group: Vec<Placement> = Vec::new();
            for index in 0..count {
                let Some(placement) = self.find_placement(plan, projection, index, &reading) else {
                    continue;
                };
                if group.first().is_none_or(|first| first.target() == placement.target()) {
                    group.push(placement);
                }
            }
            if group.is_empty() {
                return;
            }
            self.apply_placement_group(plan, projection, group);
            self.changed();
        }
    }

    /// `FindPlacement`.
    fn find_placement(
        &mut self,
        plan: &LogicalOperator,
        projection_path: &Path,
        index: usize,
        reading: &HashMap<ColumnBinding, usize>,
    ) -> Option<Placement> {
        let projection = node(plan, projection_path);
        let K::Projection {
            table_index,
            expressions,
        } = &projection.kind
        else {
            return None;
        };
        let expression = &expressions[index];
        if is_column_ref(expression) || expression.is_volatile() || can_throw(expression) {
            return None;
        }
        let (input_width, widths) = self.expression_width(expression)?;
        let exclusive_width = exclusive_input_width(reading, &widths);
        let output_width = self.binding_width(ColumnBinding::new(*table_index, index), &expression.return_type)?;
        let evaluation_width = (input_width + output_width + expression_cost(expression)) as f64;
        let current_evaluation_cost = self.cardinality(&projection.children[0]) * evaluation_width;
        if current_evaluation_cost == 0.0 {
            return None;
        }

        let mut current_expression = expression.clone();
        let mut current = projection_path.clone();
        current.push(0);
        let mut steps: Vec<(Path, usize)> = Vec::new();
        let mut transport = 0.0;
        let mut savings = 0.0;
        // beyond the first direct join, the source can be live for another path expression or join condition
        let mut can_credit_input_transport = true;
        // (steps, expression, evaluation, output transport, input transport savings)
        let mut candidates: Vec<(Vec<(Path, usize)>, BoundExpr, f64, f64, f64)> = Vec::new();
        loop {
            let op = node(plan, &current);
            match &op.kind {
                K::Filter { .. } => {
                    if op.children.len() != 1 || !bindings_belong_to(&current_expression, op) {
                        break;
                    }
                    steps.push((current.clone(), 0));
                    can_credit_input_transport = false;
                    current.push(0);
                    continue;
                }
                K::Projection {
                    table_index,
                    expressions,
                } => {
                    if !rewrite_through_projection(&mut current_expression, *table_index, expressions) {
                        break;
                    }
                    steps.push((current.clone(), 0));
                    current.push(0);
                    continue;
                }
                // the columns of a join that go on (C++ keeps them as the join's projection map)
                K::ColumnSelect { .. } => {
                    if !bindings_belong_to(&current_expression, op) {
                        break;
                    }
                    steps.push((current.clone(), 0));
                    current.push(0);
                    continue;
                }
                _ => {}
            }
            if !is_eligible_join(op) {
                break;
            }
            let left = bindings_belong_to(&current_expression, &op.children[0]);
            let right = bindings_belong_to(&current_expression, &op.children[1]);
            if left == right {
                break;
            }
            let child = usize::from(right);
            let dependencies: HashSet<ColumnBinding> = read_bindings(&current_expression)
                .unwrap_or_default()
                .into_iter()
                .map(|(b, _)| b)
                .collect();
            if operator_references_any(op, &dependencies) {
                can_credit_input_transport = false;
            }
            steps.push((current.clone(), child));
            let join_rows = self.cardinality(op);
            transport += join_rows;
            if can_credit_input_transport {
                savings += join_rows;
            }
            can_credit_input_transport = false;
            let evaluation = self.cardinality(&op.children[child]);
            candidates.push((steps.clone(), current_expression.clone(), evaluation, transport, savings));
            current.push(child);
        }

        let current_cost = current_evaluation_cost + savings * exclusive_width as f64;
        let mut best_cost = current_cost * MINIMUM_COST_RATIO;
        let mut best = None;
        for (steps, expression, evaluation, output_transport, candidate_savings) in candidates {
            let remaining_input_transport = savings - candidate_savings;
            let cost = evaluation * evaluation_width
                + output_transport * output_width as f64
                + remaining_input_transport * exclusive_width as f64;
            if cost < best_cost {
                best_cost = cost;
                best = Some(Placement {
                    expression_index: index,
                    steps,
                    expression,
                });
            }
        }
        best
    }

    /// `ApplyPlacementGroup`.
    fn apply_placement_group(&mut self, plan: &mut LogicalOperator, projection_path: &Path, group: Vec<Placement>) {
        let target_path = group[0].target();
        let upper_table = match &node(plan, projection_path).kind {
            K::Projection { table_index, .. } => *table_index,
            _ => unreachable!("a projection"),
        };
        let upper_types: Vec<LogicalType> = match &node(plan, projection_path).kind {
            K::Projection { expressions, .. } => group
                .iter()
                .map(|p| expressions[p.expression_index].return_type.clone())
                .collect(),
            _ => unreachable!("a projection"),
        };
        // `LogicalProjection::CreateIdentity` of the target, with the expressions after its columns
        let target = node(plan, &target_path);
        let old_bindings = target.column_bindings();
        let old_types = target.types();
        let lower_index = self.generate_table_index();
        let mut expressions: Vec<BoundExpr> = old_bindings
            .iter()
            .zip(&old_types)
            .map(|(binding, ty)| BoundExpr::column_ref(*binding, ty.clone(), String::new()))
            .collect();
        expressions.extend(group.iter().map(|p| p.expression.clone()));
        let slot = node_mut(plan, &target_path);
        let child = std::mem::replace(slot, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *slot = LogicalOperator::new(
            K::Projection {
                table_index: lower_index,
                expressions,
            },
            vec![child],
        );

        let mut replacements = HashMap::new();
        for (at, binding) in old_bindings.iter().enumerate() {
            let new = ColumnBinding::new(lower_index, at);
            replacements.insert(*binding, new);
            self.statistics.copy(*binding, new);
        }
        let mut computed: Vec<ColumnBinding> = Vec::with_capacity(group.len());
        for (at, placement) in group.iter().enumerate() {
            let binding = ColumnBinding::new(lower_index, old_bindings.len() + at);
            self.statistics
                .copy(ColumnBinding::new(upper_table, placement.expression_index), binding);
            computed.push(binding);
        }

        // the operators between the projection and the target read the columns of the new projection,
        // and a projection among them passes on the computed ones
        for (step_path, _) in group[0].steps.iter().rev() {
            let op = node_mut(plan, step_path);
            replace_own_bindings(op, &replacements);
            // `ExposeBinding`
            if let K::ColumnSelect { keep } = &mut op.kind {
                keep.extend(computed.iter().copied());
            }
            if let K::Projection {
                table_index,
                expressions,
            } = &mut op.kind
            {
                for (binding, ty) in computed.iter_mut().zip(&upper_types) {
                    expressions.push(BoundExpr::column_ref(*binding, ty.clone(), String::new()));
                    let new = ColumnBinding::new(*table_index, expressions.len() - 1);
                    self.statistics.copy(*binding, new);
                    *binding = new;
                }
            }
        }
        let upper = node_mut(plan, projection_path);
        replace_own_bindings(upper, &replacements);
        if let K::Projection { expressions, .. } = &mut upper.kind {
            for (placement, binding) in group.iter().zip(&computed) {
                let expr = &mut expressions[placement.expression_index];
                *expr = BoundExpr::column_ref(*binding, expr.return_type.clone(), expr.alias.clone());
            }
        }
    }

    // ---- lifting above joins

    /// `LiftFirstProfitableExpression` from the root, applied.
    fn lift_first_profitable(&mut self, plan: &mut LogicalOperator) -> bool {
        let Some(placement) = self.find_first_lift(plan, plan, &mut Vec::new(), &mut Vec::new()) else {
            return false;
        };
        self.apply_lift(plan, placement);
        self.changed();
        true
    }

    fn find_first_lift(
        &mut self,
        plan: &LogicalOperator,
        op: &LogicalOperator,
        path: &mut Path,
        ancestors: &mut Vec<(Path, usize)>,
    ) -> Option<LiftPlacement> {
        for (at, child) in op.children.iter().enumerate() {
            ancestors.push((path.clone(), at));
            path.push(at);
            let found = self.find_first_lift(plan, child, path, ancestors);
            path.pop();
            ancestors.pop();
            if found.is_some() {
                return found;
            }
        }
        let K::Projection { expressions, .. } = &op.kind else {
            return None;
        };
        let reading = count_reading_expressions(expressions);
        (0..expressions.len()).find_map(|index| self.find_lift(plan, path, index, ancestors, &reading))
    }

    /// `FindLiftPlacement`.
    fn find_lift(
        &mut self,
        plan: &LogicalOperator,
        projection_path: &Path,
        index: usize,
        ancestors: &[(Path, usize)],
        reading: &HashMap<ColumnBinding, usize>,
    ) -> Option<LiftPlacement> {
        let projection = node(plan, projection_path);
        let K::Projection {
            table_index,
            expressions,
        } = &projection.kind
        else {
            return None;
        };
        let expression = &expressions[index];
        if is_column_ref(expression) || expression.is_volatile() || can_throw(expression) {
            return None;
        }
        let (input_width, dependencies) = self.expression_width(expression)?;
        let source_binding = ColumnBinding::new(*table_index, index);
        let exclusive_width = exclusive_input_width(reading, &dependencies);

        let mut current: HashSet<ColumnBinding> = HashSet::from([source_binding]);
        let mut steps: Vec<LiftStep> = Vec::new();
        // (steps, evaluation, input transport, result bindings, target bindings, target types)
        type LiftOption = (usize, f64, f64, HashSet<ColumnBinding>, Vec<ColumnBinding>, Vec<LogicalType>);
        let mut options: Vec<LiftOption> = Vec::new();
        let mut transport = 0.0;
        for (ancestor_path, _) in ancestors.iter().rev() {
            let op = node(plan, ancestor_path);
            match &op.kind {
                K::Projection {
                    table_index,
                    expressions,
                } => {
                    // `TraceProjectionResult`
                    let mut projected = HashSet::new();
                    let mut traced = true;
                    for (at, expr) in expressions.iter().enumerate() {
                        if !references_any(expr, &current) {
                            continue;
                        }
                        match &expr.kind {
                            BoundExprKind::ColumnRef { binding, depth: 0 } if current.contains(binding) => {
                                projected.insert(ColumnBinding::new(*table_index, at));
                            }
                            _ => traced = false,
                        }
                    }
                    if !traced || projected.is_empty() {
                        break;
                    }
                    current = projected;
                }
                K::Filter { .. } => {
                    if op.children.len() != 1 || operator_references_any(op, &current) {
                        break;
                    }
                    current = retained(op, &current);
                    if current.is_empty() {
                        break;
                    }
                }
                K::ColumnSelect { .. } => {
                    current = retained(op, &current);
                    if current.is_empty() {
                        break;
                    }
                }
                _ if is_eligible_join(op) => {
                    if operator_references_any(op, &current) {
                        break;
                    }
                    current = retained(op, &current);
                    if current.is_empty() {
                        break;
                    }
                }
                _ => break,
            }
            steps.push(LiftStep {
                path: ancestor_path.clone(),
                result_bindings: current.clone(),
            });
            if !is_eligible_join(op) {
                continue;
            }
            let rows = self.cardinality(op);
            transport += rows;
            options.push((steps.len(), rows, transport, current.clone(), op.column_bindings(), op.types()));
        }
        if options.is_empty() {
            return None;
        }
        let mut output_width = self.binding_width(source_binding, &expression.return_type);
        for option in &options {
            for binding in &option.3 {
                if output_width.is_none() {
                    output_width = self.binding_width(*binding, &expression.return_type);
                }
            }
        }
        let output_width = output_width? as f64;
        let evaluation_width = input_width as f64 + output_width + expression_cost(expression) as f64;
        let source_rows = self.cardinality(&projection.children[0]);
        let current_cost = source_rows * evaluation_width + transport * output_width;
        if current_cost == 0.0 {
            return None;
        }
        let mut best_cost = current_cost * MINIMUM_COST_RATIO;
        let mut best = None;
        for (at, option) in options.iter().enumerate() {
            let remaining_output_transport = transport - option.2;
            let cost =
                option.1 * evaluation_width + option.2 * exclusive_width as f64 + remaining_output_transport * output_width;
            if cost < best_cost {
                best_cost = cost;
                best = Some(at);
            }
        }
        let (length, _, _, result_bindings, target_bindings, target_types) = options.swap_remove(best?);
        steps.truncate(length);
        Some(LiftPlacement {
            source: projection_path.clone(),
            expression_index: index,
            source_binding,
            dependencies: dependencies.into_iter().map(|(b, ty, _)| (b, ty)).collect(),
            steps,
            result_bindings,
            target_bindings,
            target_types,
        })
    }

    /// `RebuildProjection`: the projection at `path` without the columns of `removed`, passing on
    /// the dependencies, under a new table index.
    fn rebuild_projection(
        &mut self,
        plan: &mut LogicalOperator,
        path: &Path,
        removed: &HashSet<ColumnBinding>,
        dependencies: &mut [Dependency],
        replacements: &mut Vec<(ColumnBinding, ColumnBinding)>,
        lowest_rebuilt: &mut Option<usize>,
    ) {
        let projection = node_mut(plan, path);
        let old_bindings = projection.column_bindings();
        let K::Projection { expressions, .. } = &mut projection.kind else {
            unreachable!("a projection")
        };
        let mut kept = Vec::new();
        let mut dependency_indexes: Vec<Option<usize>> = vec![None; dependencies.len()];
        let mut retained_bindings = Vec::new();
        for (expr, old) in std::mem::take(expressions).into_iter().zip(old_bindings) {
            if removed.contains(&old) {
                continue;
            }
            let new_index = kept.len();
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
                // the expressions still read the columns of projections rebuilt below: they are
                // replaced once the expression is in place
                let current = resolve(*binding, replacements);
                for (dependency, at) in dependencies.iter().zip(dependency_indexes.iter_mut()) {
                    if at.is_none() && current == dependency.current && expr.return_type == dependency.ty {
                        *at = Some(new_index);
                    }
                }
            }
            kept.push(expr);
            retained_bindings.push((old, new_index));
        }
        for (dependency, at) in dependencies.iter().zip(dependency_indexes.iter_mut()) {
            if at.is_none() {
                *at = Some(kept.len());
                kept.push(BoundExpr::column_ref(dependency.current, dependency.ty.clone(), String::new()));
            }
        }
        let table_index = self.generate_table_index();
        let projection = node_mut(plan, path);
        let child = projection.children.remove(0);
        *projection = LogicalOperator::new(
            K::Projection {
                table_index,
                expressions: kept,
            },
            vec![child],
        );
        for (old, at) in retained_bindings {
            let new = ColumnBinding::new(table_index, at);
            replacements.push((old, new));
            self.statistics.copy(old, new);
        }
        for (dependency, at) in dependencies.iter_mut().zip(dependency_indexes) {
            let new = ColumnBinding::new(table_index, at.expect("set above"));
            self.statistics.copy(dependency.current, new);
            dependency.current = new;
        }
        lowest_rebuilt.get_or_insert(table_index);
    }

    /// `ApplyLiftPlacement`.
    fn apply_lift(&mut self, plan: &mut LogicalOperator, placement: LiftPlacement) {
        let mut expression = match &mut node_mut(plan, &placement.source).kind {
            K::Projection { expressions, .. } => expressions[placement.expression_index].clone(),
            _ => unreachable!("a projection"),
        };
        let mut dependencies: Vec<Dependency> = placement
            .dependencies
            .iter()
            .map(|(binding, ty)| Dependency {
                original: *binding,
                current: *binding,
                ty: ty.clone(),
            })
            .collect();
        let mut replacements = Vec::new();
        let mut lowest_rebuilt = None;
        let source_binding = HashSet::from([placement.source_binding]);
        self.rebuild_projection(
            plan,
            &placement.source,
            &source_binding,
            &mut dependencies,
            &mut replacements,
            &mut lowest_rebuilt,
        );
        for step in &placement.steps {
            // `ExposeBinding`: the dependencies go on where the result did
            if let K::ColumnSelect { keep } = &mut node_mut(plan, &step.path).kind {
                keep.retain(|binding| !step.result_bindings.contains(binding));
                // what it keeps of the projections rebuilt below, by their new columns (C++ remaps the
                // projection map of the join here)
                for binding in keep.iter_mut() {
                    *binding = resolve(*binding, &replacements);
                }
                for dependency in &dependencies {
                    if !keep.contains(&dependency.current) {
                        keep.push(dependency.current);
                    }
                }
                continue;
            }
            if matches!(node(plan, &step.path).kind, K::Projection { .. }) {
                self.rebuild_projection(
                    plan,
                    &step.path,
                    &step.result_bindings,
                    &mut dependencies,
                    &mut replacements,
                    &mut lowest_rebuilt,
                );
            }
        }

        // `RewriteLiftedExpression`
        expression.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                && let Some(dependency) = dependencies.iter().find(|d| d.original == *binding)
            {
                *binding = dependency.current;
            }
        });
        let target_path = placement.steps.last().expect("a step").path.clone();
        let target = node(plan, &target_path);
        let current_bindings = target.column_bindings();
        let current_types = target.types();

        // the compute projection: the columns of the target the result keeps, and the expression
        let mut compute: Vec<BoundExpr> = Vec::new();
        let mut inputs: Vec<ColumnBinding> = Vec::new();
        let mut restore_indexes: Vec<Option<usize>> = Vec::with_capacity(placement.target_bindings.len());
        for old in &placement.target_bindings {
            if placement.result_bindings.contains(old) {
                restore_indexes.push(None);
                continue;
            }
            let current = resolve(*old, &replacements);
            let Some(position) = current_bindings.iter().position(|b| *b == current) else {
                unreachable!("projection placement lost target binding {current:?}");
            };
            let at = match inputs.iter().position(|b| *b == current) {
                Some(at) => at,
                None => {
                    inputs.push(current);
                    compute.push(BoundExpr::column_ref(current, current_types[position].clone(), String::new()));
                    inputs.len() - 1
                }
            };
            restore_indexes.push(Some(at));
        }
        let candidate = compute.len();
        compute.push(expression);
        let compute_index = self.generate_table_index();
        let restore_index = self.generate_table_index();
        let restore: Vec<BoundExpr> = restore_indexes
            .iter()
            .zip(&placement.target_types)
            .map(|(at, ty)| {
                BoundExpr::column_ref(
                    ColumnBinding::new(compute_index, at.unwrap_or(candidate)),
                    ty.clone(),
                    String::new(),
                )
            })
            .collect();
        let slot = node_mut(plan, &target_path);
        let target = std::mem::replace(slot, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *slot = LogicalOperator::new(
            K::Projection {
                table_index: restore_index,
                expressions: restore,
            },
            vec![LogicalOperator::new(
                K::Projection {
                    table_index: compute_index,
                    expressions: compute,
                },
                vec![target],
            )],
        );
        for (at, input) in inputs.iter().enumerate() {
            self.statistics.copy(*input, ColumnBinding::new(compute_index, at));
        }
        self.statistics
            .copy(placement.source_binding, ColumnBinding::new(compute_index, candidate));

        // with the restore projection in place, what read the rebuilt projections reads their new columns
        let rebuilt: HashMap<ColumnBinding, ColumnBinding> = replacements
            .iter()
            .filter_map(|(old, _)| {
                let last = resolve(*old, &replacements);
                (last != *old).then_some((*old, last))
            })
            .collect();
        replace_bindings(plan, lowest_rebuilt, &rebuilt);

        // and what read the target reads the restore projection
        let mut restored = HashMap::new();
        for (at, old) in placement.target_bindings.iter().enumerate() {
            let binding = ColumnBinding::new(restore_index, at);
            restored.insert(*old, binding);
            if !placement.result_bindings.contains(old) {
                let current = resolve(*old, &replacements);
                if current != *old {
                    restored.insert(current, binding);
                }
            }
            let source = ColumnBinding::new(compute_index, restore_indexes[at].unwrap_or(candidate));
            self.statistics.copy(source, binding);
        }
        replace_bindings(plan, Some(restore_index), &restored);
    }
}
