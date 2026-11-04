// Ported from duckdb/src/execution/physical_plan_generator.cpp, duckdb/src/execution/
// physical_plan/plan_*.cpp and duckdb/src/parallel/{pipeline,meta_pipeline,
// pipeline_executor,executor}.cpp @ rust-port-baseline
//
//! Execution: turning a logical plan into physical operators, splitting those into pipelines
//! (a source, streaming operators, a sink) and running the pipelines in dependency order on
//! the calling thread.

pub mod expression_executor;
pub mod join;
pub mod physical;

use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::Result;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::vector::DataChunk;
use parking_lot::Mutex;
use physical::*;

pub mod constraints;

/// `PhysicalPlanGenerator::CreatePlan`.
pub fn create_plan(op: LogicalOperator) -> Box<dyn PhysicalOperator> {
    let types = op.types();
    let mut children: Vec<Box<dyn PhysicalOperator>> = op.children.into_iter().map(create_plan).collect();
    match op.kind {
        LogicalOperatorKind::Get { table, column_ids, .. } => Box::new(PhysicalTableScan {
            table,
            column_ids,
            types,
        }),
        LogicalOperatorKind::Range {
            start,
            increment,
            count,
            ..
        } => Box::new(PhysicalRange {
            types,
            start,
            increment,
            count,
        }),
        LogicalOperatorKind::CreateTableAs {
            schema,
            name,
            columns,
            on_conflict,
        } => {
            let types_of_columns: Vec<_> = columns.iter().map(|c| c.ty.clone()).collect();
            Box::new(PhysicalCreateTableAs {
                types,
                schema,
                table_name: name,
                columns,
                on_conflict,
                children,
                data: Mutex::new(duckrs_common::column_data::ColumnDataCollection::new(types_of_columns)),
            })
        }
        LogicalOperatorKind::EmptyResult { .. } => Box::new(PhysicalEmptyResult { types }),
        LogicalOperatorKind::DummyScan { .. } => Box::new(PhysicalDummyScan { types }),
        LogicalOperatorKind::ExpressionGet { expressions, .. } => {
            Box::new(PhysicalExpressionScan { types, expressions })
        }
        LogicalOperatorKind::Filter { mut expressions } => {
            // several conditions all have to hold
            let condition = if expressions.len() == 1 {
                expressions.pop().unwrap()
            } else {
                crate::planner::expression::BoundExpr::new(
                    crate::planner::expression::BoundExprKind::Conjunction {
                        and: true,
                        children: expressions,
                    },
                    duckrs_common::types::LogicalType::BOOLEAN,
                )
            };
            Box::new(PhysicalFilter {
                types,
                condition,
                children,
            })
        }
        LogicalOperatorKind::Aggregate { groups, aggregates, .. } => {
            Box::new(PhysicalAggregate::new(types, groups, aggregates, children))
        }
        LogicalOperatorKind::Order { orders } => Box::new(PhysicalOrder::new(orders, children.remove(0))),
        LogicalOperatorKind::Limit { limit, offset } => Box::new(PhysicalLimit {
            types,
            children,
            remaining: Mutex::new((offset, limit)),
        }),
        LogicalOperatorKind::Distinct { targets } => Box::new(PhysicalDistinct::new(&targets, children.remove(0))),
        LogicalOperatorKind::Delete { table, returning, .. } => {
            Box::new(PhysicalDelete::new(types, children, table, returning))
        }
        LogicalOperatorKind::Update {
            table,
            columns,
            returning,
            ..
        } => Box::new(PhysicalUpdate::new(types, children, table, columns, returning)),
        LogicalOperatorKind::Union { .. } => Box::new(PhysicalUnion::new(types, children)),
        LogicalOperatorKind::RecursiveCte {
            union_all,
            working,
            recursive,
            ..
        } => Box::new(PhysicalRecursiveCte::new(
            types, children, union_all, working, *recursive,
        )),
        LogicalOperatorKind::WorkingTable { working, .. } => Box::new(PhysicalWorkingTable { types, working }),
        LogicalOperatorKind::LateralJoin {
            join_type,
            right,
            bindings,
            correlated,
            predicate,
        } => Box::new(PhysicalLateralJoin {
            types,
            children,
            join_type,
            right: *right,
            bindings,
            correlated,
            predicate,
            results: Mutex::default(),
        }),
        LogicalOperatorKind::ExceptIntersect { except, all, .. } => {
            Box::new(PhysicalExceptIntersect::new(types, children, except, all))
        }
        LogicalOperatorKind::Unnest { expressions, .. } => Box::new(PhysicalUnnest {
            types,
            expressions,
            children,
        }),
        LogicalOperatorKind::Projection { expressions, .. } => Box::new(PhysicalProjection {
            types,
            expressions,
            children,
        }),
        LogicalOperatorKind::CrossProduct => Box::new(join::PhysicalCrossProduct::new(types, children)),
        LogicalOperatorKind::Join {
            join_type,
            conditions,
            predicate,
        } => Box::new(join::PhysicalJoin::new(
            types, join_type, conditions, predicate, children,
        )),
        LogicalOperatorKind::Insert {
            table,
            column_index_map,
        } => Box::new(PhysicalInsert {
            types,
            table,
            column_index_map,
            children: std::mem::take(&mut children),
            inserted: Mutex::new(0),
            first_new_row: Mutex::new(None),
        }),
        LogicalOperatorKind::CreateTable {
            schema,
            name,
            columns,
            constraints,
            on_conflict,
        } => Box::new(PhysicalCreateTable {
            types,
            schema,
            table_name: name,
            columns,
            constraints,
            on_conflict,
        }),
    }
}

/// A pipeline (`Pipeline`): chunks flow from the source through the operators into the sink.
/// Several pipelines can feed one sink, and only the last of them finishes it.
struct Pipeline<'a> {
    source: &'a dyn PhysicalOperator,
    operators: Vec<&'a dyn PhysicalOperator>,
    sink: &'a dyn PhysicalOperator,
    /// which of the sink's children this pipeline comes from
    input: usize,
    finalize: bool,
}

impl Pipeline<'_> {
    /// Pushes `chunk` through the operators from `index` on, into the sink, as many times as
    /// each operator has output for it (`PipelineExecutor::Execute`).
    fn push(&self, chunk: DataChunk, index: usize, states: &mut [OperatorState]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let Some(op) = self.operators.get(index) else {
            return self.sink.sink_input(self.input, &chunk);
        };
        loop {
            let (output, result) = op.execute_stateful(&chunk, &mut states[index])?;
            self.push(output, index + 1, states)?;
            if result == OperatorResult::NeedMoreInput || self.is_finished() {
                return Ok(());
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.operators.iter().any(|op| op.is_finished())
    }

    /// `PipelineExecutor::Execute` + `PipelineFinishEvent`.
    fn run(&self) -> Result<()> {
        let mut state = self.source.source_state();
        let mut operator_states: Vec<OperatorState> = self.operators.iter().map(|op| op.operator_state()).collect();
        while let Some(chunk) = self.source.get_data(&mut state)? {
            self.push(chunk, 0, &mut operator_states)?;
            if self.is_finished() {
                break;
            }
        }
        if self.finalize {
            self.sink.finalize()?;
        }
        Ok(())
    }
}

/// `MetaPipeline::Build` / `PhysicalOperator::BuildPipelines`: collects, children first, the
/// pipelines that feed `sink` and the ones they depend on.
fn build_pipelines<'a>(sink: &'a dyn PhysicalOperator, pipelines: &mut Vec<Pipeline<'a>>) {
    for input in sink.sink_children() {
        let mut operators = Vec::new();
        let mut current = sink.children()[input].as_ref();
        // walk down through the streaming operators to this pipeline's source
        while !current.is_source() {
            operators.push(current);
            if current.is_sink() {
                // a join: its build side runs first
                build_pipelines(current, pipelines);
            }
            current = current.children()[0].as_ref();
        }
        if current.is_sink() {
            // the source is also a sink (e.g. INSERT): its own input pipeline runs first
            build_pipelines(current, pipelines);
        }
        operators.reverse();
        // the rows of RIGHT and FULL joins that nothing matched go through the operators above them
        let unmatched: Vec<Pipeline<'a>> = operators
            .iter()
            .enumerate()
            .filter(|(_, op)| op.has_unmatched_source())
            .map(|(position, op)| Pipeline {
                source: *op,
                operators: operators[position + 1..].to_vec(),
                sink,
                input,
                finalize: false,
            })
            .collect();
        pipelines.push(Pipeline {
            source: current,
            operators,
            sink,
            input,
            finalize: false,
        });
        pipelines.extend(unmatched);
    }
    pipelines.last_mut().expect("just added").finalize = true;
}

/// Runs a plan with resolved column bindings, and returns its rows.
pub fn run_plan(plan: LogicalOperator) -> Result<ColumnDataCollection> {
    let collector = PhysicalResultCollector::new(create_plan(plan));
    execute_plan(&collector)?;
    Ok(collector.result.into_inner())
}

/// `Executor::Initialize` + `Execute`: runs a plan whose root is a sink.
pub fn execute_plan(root: &dyn PhysicalOperator) -> Result<()> {
    let mut pipelines = Vec::new();
    build_pipelines(root, &mut pipelines);
    for pipeline in &pipelines {
        pipeline.run()?;
    }
    Ok(())
}
