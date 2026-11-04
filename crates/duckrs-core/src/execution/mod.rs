// Ported from duckdb/src/execution/physical_plan_generator.cpp, duckdb/src/execution/
// physical_plan/plan_*.cpp and duckdb/src/parallel/{pipeline,meta_pipeline,
// pipeline_executor,executor}.cpp @ rust-port-baseline
//
//! Execution: turning a logical plan into physical operators, splitting those into pipelines
//! (a source, streaming operators, a sink) and running the pipelines in dependency order on
//! the calling thread.

pub mod expression_executor;
pub mod physical;

use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::Result;
use parking_lot::Mutex;
use physical::*;

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
        LogicalOperatorKind::Projection { expressions, .. } => Box::new(PhysicalProjection {
            types,
            expressions,
            children,
        }),
        LogicalOperatorKind::Insert {
            table,
            column_index_map,
        } => Box::new(PhysicalInsert {
            types,
            table,
            column_index_map,
            children: std::mem::take(&mut children),
            inserted: Mutex::new(0),
        }),
        LogicalOperatorKind::CreateTable {
            schema,
            name,
            columns,
            on_conflict,
        } => Box::new(PhysicalCreateTable {
            types,
            schema,
            table_name: name,
            columns,
            on_conflict,
        }),
    }
}

/// A pipeline (`Pipeline`): chunks flow from the source through the operators into the sink.
struct Pipeline<'a> {
    source: &'a dyn PhysicalOperator,
    operators: Vec<&'a dyn PhysicalOperator>,
    sink: &'a dyn PhysicalOperator,
}

impl Pipeline<'_> {
    /// `PipelineExecutor::Execute` + `PipelineFinishEvent`.
    fn run(&self) -> Result<()> {
        let mut state = self.source.source_state();
        while let Some(mut chunk) = self.source.get_data(&mut state)? {
            for op in &self.operators {
                if chunk.is_empty() {
                    break;
                }
                chunk = op.execute(chunk)?;
            }
            if !chunk.is_empty() {
                self.sink.sink(&chunk)?;
            }
        }
        self.sink.finalize()
    }
}

/// `MetaPipeline::Build` / `PhysicalOperator::BuildPipelines`: collects, children first, the
/// pipeline that feeds `sink` and the pipelines its source depends on.
fn build_pipelines<'a>(sink: &'a dyn PhysicalOperator, pipelines: &mut Vec<Pipeline<'a>>) {
    let mut operators = Vec::new();
    let mut current = sink.children()[0].as_ref();
    // walk down through the streaming operators to this pipeline's source
    while !current.is_source() {
        operators.push(current);
        current = current.children()[0].as_ref();
    }
    if current.is_sink() {
        // the source is also a sink (e.g. INSERT): its own input pipeline runs first
        build_pipelines(current, pipelines);
    }
    operators.reverse();
    pipelines.push(Pipeline {
        source: current,
        operators,
        sink,
    });
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
