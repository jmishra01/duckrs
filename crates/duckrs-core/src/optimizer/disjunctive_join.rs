// Ported from duckdb/src/optimizer/disjunctive_join_rewriter.cpp and
// duckdb/src/planner/logical_operator_repeatability.cpp @ rust-port-baseline
//
//! `DisjunctiveJoinRewriter`: a join on an OR of equalities between its sides (`a = x OR b = y`)
//! becomes a hash join for each equality, over materialized CTEs of the two sides. The matches of
//! an equality that an earlier one already found are left out, so that each pair comes once; the
//! outer, SEMI and ANTI joins add their unmatched rows through ANTI joins.

use super::common_subplan::{max_table_index, visit_bindings};
use super::rewriter::can_throw;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{JoinAlgorithm, JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Bounds the quadratic exclusions and the size of the plan they make.
const MAX_BRANCH_COUNT: usize = 16;

/// One equality of the OR: `left` reads the left side, `right` the right side.
#[derive(Clone)]
struct Branch {
    left: BoundExpr,
    right: BoundExpr,
    comparison: ComparisonOp,
}

/// A side of the join as a CTE.
struct CteInfo {
    table_index: usize,
    types: Vec<LogicalType>,
    original_bindings: Vec<ColumnBinding>,
    data: Arc<Mutex<ColumnDataCollection>>,
}

struct Rewriter {
    next_table_index: usize,
}

/// Runs the pass over a plan.
pub fn rewrite(mut plan: LogicalOperator) -> LogicalOperator {
    let mut rewriter = Rewriter {
        next_table_index: max_table_index(&plan) + 1,
    };
    rewriter.optimize(&mut plan);
    plan
}

impl Rewriter {
    fn new_table_index(&mut self) -> usize {
        let index = self.next_table_index;
        self.next_table_index += 1;
        index
    }

    /// `OptimizeInternal`: rewrites the joins below and then this one, and returns the columns that
    /// moved, which the operators above read under their new bindings.
    fn optimize(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let mut replacements = HashMap::new();
        for child in &mut op.children {
            replacements.extend(self.optimize(child));
        }
        if !replacements.is_empty() {
            visit_bindings(&mut op.kind, &mut |binding, depth| {
                if depth == 0
                    && let Some(new) = replacements.get(binding)
                {
                    *binding = *new;
                }
            });
        }
        let K::Join {
            join_type,
            conditions,
            predicate: Some(predicate),
            ..
        } = &op.kind
        else {
            return replacements;
        };
        if !conditions.is_empty() {
            return replacements;
        }
        let join_type = *join_type;
        let left_tables: HashSet<usize> = op.children[0].column_bindings().iter().map(|b| b.table_index).collect();
        let right_tables: HashSet<usize> = op.children[1].column_bindings().iter().map(|b| b.table_index).collect();
        let Some(branches) = should_rewrite(join_type, predicate, &left_tables, &right_tables) else {
            return replacements;
        };

        let original_bindings = op.column_bindings();
        let original_types = op.types();
        let right_child = op.children.pop().expect("a join has two children");
        let left_child = op.children.pop().expect("a join has two children");
        let left_cte = self.cte_info(&left_child);
        let right_cte = self.cte_info(&right_child);

        let epilogue = match join_type {
            JoinType::Inner => self.build_inner_join(&left_cte, &right_cte, &branches),
            JoinType::Left => self.build_left_join(&left_cte, &right_cte, &branches),
            JoinType::Right => self.build_left_join(&right_cte, &left_cte, &swap_branches(&branches)),
            JoinType::Outer => self.build_full_join(&left_cte, &right_cte, &branches),
            JoinType::Semi => self.build_semi_join(&left_cte, &right_cte, &branches),
            JoinType::Anti => {
                let probe = self.new_table_index();
                self.anti_join_chain(&left_cte, &right_cte, &branches, probe)
            }
            _ => unreachable!("should_rewrite only accepts these"),
        };
        let epilogue = self.normalize_output(
            epilogue,
            &original_bindings,
            &original_types,
            &left_cte,
            &right_cte,
            join_type,
            &mut replacements,
        );

        // every branch must see the same rows of a side unless reading it again gives them
        let right_always = !subtree_is_repeatable(&right_child);
        let left_always = !subtree_is_repeatable(&left_child);
        let epilogue = LogicalOperator::new(
            K::MaterializedCte {
                name: "rhs_cte".to_string(),
                table_index: right_cte.table_index,
                data: right_cte.data.clone(),
                always: right_always,
            },
            vec![right_child, epilogue],
        );
        *op = LogicalOperator::new(
            K::MaterializedCte {
                name: "lhs_cte".to_string(),
                table_index: left_cte.table_index,
                data: left_cte.data.clone(),
                always: left_always,
            },
            vec![left_child, epilogue],
        );
        debug_assert_eq!(op.types(), original_types);
        replacements
    }

    fn cte_info(&mut self, child: &LogicalOperator) -> CteInfo {
        let types = child.types();
        CteInfo {
            table_index: self.new_table_index(),
            data: Arc::new(Mutex::new(ColumnDataCollection::new(types.clone()))),
            types,
            original_bindings: child.column_bindings(),
        }
    }

    fn build_inner_join(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let children = self.matched_branches(left, right, branches);
        self.union_all(children, left, right)
    }

    fn build_left_join(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let mut children = self.matched_branches(left, right, branches);
        children.push(self.unmatched_probe_side(left, right, branches));
        self.union_all(children, left, right)
    }

    fn build_full_join(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let mut children = self.matched_branches(left, right, branches);
        children.push(self.unmatched_probe_side(left, right, branches));
        children.push(self.unmatched_build_side(left, right, branches));
        self.union_all(children, left, right)
    }

    /// `BuildSemiJoin`: a MARK join for each branch, and the rows that one of them marks.
    fn build_semi_join(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let probe_ref = self.new_table_index();
        let mut current = cte_ref(left, probe_ref);
        let probe_map = replacer(left, probe_ref);
        let mut markers = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            let build_ref = self.new_table_index();
            let mark_index = self.new_table_index();
            let build_map = replacer(right, build_ref);
            current = LogicalOperator::new(
                K::MarkJoin {
                    table_index: mark_index,
                    conditions: vec![join_condition(branch, &probe_map, &build_map)],
                    predicate: None,
                },
                vec![current, cte_ref(right, build_ref)],
            );
            markers.push(BoundExpr::column_ref(
                ColumnBinding::new(mark_index, 0),
                LogicalType::BOOLEAN,
                format!("mark_{i}"),
            ));
        }
        LogicalOperator::new(
            K::Filter {
                expressions: vec![disjunction(markers)],
            },
            vec![current],
        )
    }

    /// `CreateBranchJoin`: the pairs that branch `at` matches and no earlier branch does.
    fn branch_join(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch], at: usize) -> LogicalOperator {
        let (left_ref, right_ref) = (self.new_table_index(), self.new_table_index());
        let (left_map, right_map) = (replacer(left, left_ref), replacer(right, right_ref));
        let condition = join_condition(&branches[at], &left_map, &right_map);
        let mut exclusions = Vec::new();
        for previous in &branches[..at] {
            let previous = join_condition(previous, &left_map, &right_map);
            let mut options = Vec::new();
            if previous.comparison == ComparisonOp::Equal {
                // keeps UNKNOWN apart, with NULL checks that statistics can simplify
                options.push(is_null(previous.left.clone()));
                options.push(is_null(previous.right.clone()));
            }
            options.push(BoundExpr::new(
                BoundExprKind::Comparison {
                    op: previous.comparison.negate(),
                    left: Box::new(previous.left),
                    right: Box::new(previous.right),
                },
                LogicalType::BOOLEAN,
            ));
            exclusions.push(disjunction(options));
        }
        LogicalOperator::new(
            K::Join {
                join_type: JoinType::Inner,
                conditions: vec![condition],
                predicate: (!exclusions.is_empty()).then(|| super::conjunction_of(exclusions)),
                algorithm: JoinAlgorithm::default(),
            },
            vec![cte_ref(left, left_ref), cte_ref(right, right_ref)],
        )
    }

    fn matched_branches(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> Vec<LogicalOperator> {
        (0..branches.len())
            .map(|at| self.branch_join(left, right, branches, at))
            .collect()
    }

    /// `CreateUnmatchedProbeSide`: the probe rows no branch matches, with NULLs for the build side.
    fn unmatched_probe_side(&mut self, probe: &CteInfo, build: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let probe_ref = self.new_table_index();
        let chain = self.anti_join_chain(probe, build, branches, probe_ref);
        let mut expressions = column_refs(probe_ref, &probe.types);
        expressions.extend(nulls(&build.types));
        self.projection(chain, expressions)
    }

    /// `CreateUnmatchedBuildSide`: the right rows no branch matches, with NULLs for the left side.
    fn unmatched_build_side(&mut self, left: &CteInfo, right: &CteInfo, branches: &[Branch]) -> LogicalOperator {
        let right_ref = self.new_table_index();
        let chain = self.anti_join_chain(right, left, &swap_branches(branches), right_ref);
        let mut expressions = nulls(&left.types);
        expressions.extend(column_refs(right_ref, &right.types));
        self.projection(chain, expressions)
    }

    /// `CreateAntiJoinChain`: the probe rows that none of the branches matches.
    fn anti_join_chain(
        &mut self,
        probe: &CteInfo,
        build: &CteInfo,
        branches: &[Branch],
        probe_ref: usize,
    ) -> LogicalOperator {
        let mut current = cte_ref(probe, probe_ref);
        let probe_map = replacer(probe, probe_ref);
        for branch in branches {
            let build_ref = self.new_table_index();
            let build_map = replacer(build, build_ref);
            current = LogicalOperator::new(
                K::Join {
                    join_type: JoinType::Anti,
                    conditions: vec![join_condition(branch, &probe_map, &build_map)],
                    predicate: None,
                    algorithm: JoinAlgorithm::default(),
                },
                vec![current, cte_ref(build, build_ref)],
            );
        }
        current
    }

    fn union_all(&mut self, children: Vec<LogicalOperator>, left: &CteInfo, right: &CteInfo) -> LogicalOperator {
        let mut types = left.types.clone();
        types.extend(right.types.iter().cloned());
        LogicalOperator::new(
            K::Union {
                table_index: self.new_table_index(),
                types,
            },
            children,
        )
    }

    fn projection(&mut self, child: LogicalOperator, expressions: Vec<BoundExpr>) -> LogicalOperator {
        LogicalOperator::new(
            K::Projection {
                table_index: self.new_table_index(),
                expressions,
            },
            vec![child],
        )
    }

    /// `NormalizeOutput`: the columns of the join in its order, under new bindings that the
    /// operators above read instead.
    #[allow(clippy::too_many_arguments)]
    fn normalize_output(
        &mut self,
        plan: LogicalOperator,
        original_bindings: &[ColumnBinding],
        original_types: &[LogicalType],
        left: &CteInfo,
        right: &CteInfo,
        join_type: JoinType,
        replacements: &mut HashMap<ColumnBinding, ColumnBinding>,
    ) -> LogicalOperator {
        let current = plan.column_bindings();
        let (first, second) = if join_type == JoinType::Right {
            (right, left)
        } else {
            (left, right)
        };
        let mut positions: HashMap<ColumnBinding, usize> = HashMap::new();
        for (i, binding) in first.original_bindings.iter().enumerate() {
            positions.entry(*binding).or_insert(i);
        }
        if !matches!(join_type, JoinType::Semi | JoinType::Anti) {
            for (i, binding) in second.original_bindings.iter().enumerate() {
                positions.entry(*binding).or_insert(first.original_bindings.len() + i);
            }
        }
        let expressions = original_bindings
            .iter()
            .zip(original_types)
            .map(|(binding, ty)| BoundExpr::column_ref(current[positions[binding]], ty.clone(), ""))
            .collect();
        let table_index = self.new_table_index();
        let moved: HashMap<ColumnBinding, ColumnBinding> = original_bindings
            .iter()
            .enumerate()
            .map(|(i, binding)| (*binding, ColumnBinding::new(table_index, i)))
            .collect();
        // a column that a join below moved here moves on
        for target in replacements.values_mut() {
            if let Some(new) = moved.get(target) {
                *target = *new;
            }
        }
        replacements.extend(moved);
        LogicalOperator::new(
            K::Projection {
                table_index,
                expressions,
            },
            vec![plan],
        )
    }
}

/// `ShouldRewrite`: the equalities of the join's OR, if there are at least two and nothing else.
fn should_rewrite(
    join_type: JoinType,
    predicate: &BoundExpr,
    left_tables: &HashSet<usize>,
    right_tables: &HashSet<usize>,
) -> Option<Vec<Branch>> {
    if !matches!(
        join_type,
        JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Outer | JoinType::Semi | JoinType::Anti
    ) {
        return None;
    }
    if !matches!(predicate.kind, BoundExprKind::Conjunction { and: false, .. }) {
        return None;
    }
    if predicate.is_volatile() || can_throw(predicate) {
        return None;
    }
    let mut branches = Vec::new();
    flatten_or(predicate, left_tables, right_tables, &mut branches).then_some(())?;
    (branches.len() >= 2).then_some(branches)
}

/// `FlattenOR`.
fn flatten_or(
    expr: &BoundExpr,
    left_tables: &HashSet<usize>,
    right_tables: &HashSet<usize>,
    out: &mut Vec<Branch>,
) -> bool {
    if out.len() >= MAX_BRANCH_COUNT {
        return false;
    }
    match &expr.kind {
        BoundExprKind::Conjunction { and: false, children } => children
            .iter()
            .all(|child| flatten_or(child, left_tables, right_tables, out)),
        BoundExprKind::Comparison {
            op: op @ (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom),
            left,
            right,
        } => {
            let left_side = join_side(left, left_tables, right_tables);
            let right_side = join_side(right, left_tables, right_tables);
            let (left, right) = match (left_side, right_side) {
                (Side::Left, Side::Right) => (left, right),
                (Side::Right, Side::Left) => (right, left),
                _ => return false,
            };
            out.push(Branch {
                left: (**left).clone(),
                right: (**right).clone(),
                comparison: *op,
            });
            true
        }
        _ => false,
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Side {
    None,
    Left,
    Right,
    Both,
}

/// `JoinSide::GetJoinSide`.
fn join_side(expr: &BoundExpr, left_tables: &HashSet<usize>, right_tables: &HashSet<usize>) -> Side {
    fn visit(expr: &BoundExpr, left: &HashSet<usize>, right: &HashSet<usize>, side: &mut Side) {
        let own = match &expr.kind {
            BoundExprKind::ColumnRef { binding, depth: 0 } => {
                if left.contains(&binding.table_index) {
                    Side::Left
                } else if right.contains(&binding.table_index) {
                    Side::Right
                } else {
                    Side::Both
                }
            }
            BoundExprKind::ColumnRef { .. } | BoundExprKind::Subquery { .. } => Side::Both,
            _ => Side::None,
        };
        *side = match (*side, own) {
            (current, Side::None) => current,
            (Side::None, new) => new,
            (current, new) if current == new => current,
            _ => Side::Both,
        };
        for child in expr.children() {
            visit(child, left, right, side);
        }
    }
    let mut side = Side::None;
    visit(expr, left_tables, right_tables, &mut side);
    side
}

fn swap_branches(branches: &[Branch]) -> Vec<Branch> {
    branches
        .iter()
        .map(|branch| Branch {
            left: branch.right.clone(),
            right: branch.left.clone(),
            comparison: branch.comparison,
        })
        .collect()
}

/// What the columns of a side are called in a scan `ref_index` of its CTE.
fn replacer(cte: &CteInfo, ref_index: usize) -> HashMap<ColumnBinding, ColumnBinding> {
    cte.original_bindings
        .iter()
        .enumerate()
        .rev()
        .map(|(i, binding)| (*binding, ColumnBinding::new(ref_index, i)))
        .collect()
}

fn replace(mut expr: BoundExpr, map: &HashMap<ColumnBinding, ColumnBinding>) -> BoundExpr {
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
    expr
}

fn join_condition(
    branch: &Branch,
    left: &HashMap<ColumnBinding, ColumnBinding>,
    right: &HashMap<ColumnBinding, ColumnBinding>,
) -> JoinCondition {
    JoinCondition {
        bound: None,
        left: replace(branch.left.clone(), left),
        right: replace(branch.right.clone(), right),
        comparison: branch.comparison,
        filter: None,
    }
}

/// `CreateCTERef`.
fn cte_ref(cte: &CteInfo, ref_index: usize) -> LogicalOperator {
    LogicalOperator::new(
        K::CteRef {
            table_index: ref_index,
            cte_index: cte.table_index,
            types: cte.types.clone(),
            names: (0..cte.types.len()).map(|i| format!("col_{i}")).collect(),
            data: cte.data.clone(),
        },
        Vec::new(),
    )
}

fn column_refs(table_index: usize, types: &[LogicalType]) -> Vec<BoundExpr> {
    types
        .iter()
        .enumerate()
        .map(|(i, ty)| BoundExpr::column_ref(ColumnBinding::new(table_index, i), ty.clone(), ""))
        .collect()
}

fn nulls(types: &[LogicalType]) -> Vec<BoundExpr> {
    types
        .iter()
        .map(|ty| BoundExpr::constant(Value::null(ty.clone())))
        .collect()
}

fn is_null(expr: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Operator {
            op: BoundOperator::IsNull,
            children: vec![expr],
        },
        LogicalType::BOOLEAN,
    )
}

fn disjunction(mut children: Vec<BoundExpr>) -> BoundExpr {
    if children.len() == 1 {
        return children.remove(0);
    }
    BoundExpr::new(
        BoundExprKind::Conjunction { and: false, children },
        LogicalType::BOOLEAN,
    )
}

/// `LogicalSubtreeIsRepeatable`: whether running the plan again gives the same rows.
pub(super) fn subtree_is_repeatable(op: &LogicalOperator) -> bool {
    operator_is_repeatable(op) && op.children.iter().all(subtree_is_repeatable)
}

/// `ClassifyLogicalOperatorRepeatability`.
fn operator_is_repeatable(op: &LogicalOperator) -> bool {
    let repeatable_type = match &op.kind {
        K::Projection { .. }
        | K::Filter { .. }
        | K::Aggregate { .. }
        | K::Unnest { .. }
        | K::Order { .. }
        | K::Distinct { .. }
        | K::Window { .. }
        | K::ChunkGet { .. }
        | K::ExpressionGet { .. }
        | K::DummyScan { .. }
        | K::EmptyResult { .. }
        | K::CteRef { .. }
        | K::Join { .. }
        | K::MarkJoin { .. }
        | K::CrossProduct
        | K::AsOfJoin { .. }
        | K::Union { .. }
        | K::ExceptIntersect { .. }
        | K::MaterializedCte { .. }
        | K::SecureView { .. }
        | K::ColumnSelect { .. }
        | K::Get { .. }
        | K::Range { .. }
        | K::CsvScan { .. }
        | K::JsonScan { .. }
        | K::ParquetScan { .. } => true,
        K::Sample { options } => options.repeatable,
        _ => false,
    };
    if !repeatable_type {
        return false;
    }
    let mut repeatable = true;
    let mut shallow = LogicalOperator::new(op.kind.clone(), Vec::new());
    shallow.visit_expressions(&mut |e| repeatable &= !e.is_volatile());
    if let K::Join { conditions, .. } | K::MarkJoin { conditions, .. } | K::AsOfJoin { conditions, .. } = &op.kind {
        repeatable &= conditions
            .iter()
            .all(|c| !c.left.is_volatile() && !c.right.is_volatile());
    }
    repeatable
}
