// Ported from duckdb/src/execution/physical_plan_generator.cpp, duckdb/src/execution/
// physical_plan/plan_*.cpp and duckdb/src/parallel/{pipeline,meta_pipeline,
// pipeline_executor,executor}.cpp @ rust-port-baseline
//
//! Execution: turning a logical plan into physical operators, splitting those into pipelines
//! (a source, streaming operators, a sink) and running the pipelines in dependency order on
//! the calling thread.

pub mod art;
pub mod expression_executor;
pub mod join;
pub mod physical;
pub mod zone_map;

use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::Result;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::vector::DataChunk;
use parking_lot::Mutex;
use physical::*;

pub mod alter;
pub mod constraints;
mod merge;
mod special_joins;
mod upsert;
mod window;

/// A projection does not compute what is dropped right after it.
fn create_column_select(mut op: LogicalOperator) -> Box<dyn PhysicalOperator> {
    let types = op.types();
    let LogicalOperatorKind::ColumnSelect { keep } = &op.kind else {
        unreachable!("the caller checked")
    };
    let mut child = op.children.remove(0);
    let bindings = child.column_bindings();
    let positions: Vec<usize> = keep
        .iter()
        .map(|binding| {
            bindings
                .iter()
                .position(|b| b == binding)
                .expect("a column of the child")
        })
        .collect();
    if let LogicalOperatorKind::Projection { expressions, .. } = &mut child.kind {
        *expressions = positions.iter().map(|&p| expressions[p].clone()).collect();
        return create_plan(child);
    }
    Box::new(PhysicalColumnSelect {
        types,
        positions,
        children: vec![create_plan(child)],
    })
}

/// `PhysicalPlanGenerator::CreatePlan`.
pub fn create_plan(op: LogicalOperator) -> Box<dyn PhysicalOperator> {
    if matches!(op.kind, LogicalOperatorKind::ColumnSelect { .. }) {
        return create_column_select(op);
    }
    let types = op.types();
    let mut children: Vec<Box<dyn PhysicalOperator>> = op.children.into_iter().map(create_plan).collect();
    match op.kind {
        LogicalOperatorKind::Get {
            table,
            column_ids,
            filters,
            projection,
            index_rows,
            ..
        } => Box::new(PhysicalTableScan {
            table,
            column_ids,
            projection,
            types,
            filter: (!filters.is_empty()).then(|| crate::optimizer::conjunction_of(filters)),
            index_rows,
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
        LogicalOperatorKind::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            grouping_functions,
            ..
        } => Box::new(PhysicalAggregate::new(
            types,
            groups,
            aggregates,
            &grouping_sets,
            &grouping_functions,
            children,
        )),
        LogicalOperatorKind::Window { windows, .. } => Box::new(window::PhysicalWindow::new(types, windows, children)),
        LogicalOperatorKind::Order { orders } => Box::new(PhysicalOrder::new(orders, None, children.remove(0))),
        LogicalOperatorKind::TopN { orders, limit, offset } => {
            Box::new(PhysicalOrder::new(orders, Some((limit, offset)), children.remove(0)))
        }
        LogicalOperatorKind::Limit { limit, offset } => Box::new(PhysicalLimit {
            types,
            children,
            remaining: Mutex::new((offset, limit)),
        }),
        LogicalOperatorKind::LimitPercent { percent, offset } => {
            let data = Mutex::new(ColumnDataCollection::new(types.clone()));
            Box::new(PhysicalLimitPercent {
                types,
                percent,
                offset,
                children,
                data,
            })
        }
        LogicalOperatorKind::Sample { options } => Box::new(PhysicalSample::new(options, children.remove(0))),
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
        LogicalOperatorKind::MergeInto {
            table,
            actions,
            row_id,
            marker,
            returning,
            ..
        } => {
            let input = Mutex::new(ColumnDataCollection::new(children[0].types().to_vec()));
            let output = Mutex::new(ColumnDataCollection::new(types.clone()));
            Box::new(merge::PhysicalMergeInto {
                types,
                children,
                table,
                actions,
                row_id,
                marker,
                returning,
                input,
                output,
            })
        }
        LogicalOperatorKind::Union { .. } => Box::new(PhysicalUnion::new(types, children)),
        LogicalOperatorKind::RecursiveCte {
            union_all,
            working,
            recurring,
            key,
            recursive,
            ..
        } => Box::new(PhysicalRecursiveCte::new(
            types, children, union_all, working, recurring, key, *recursive,
        )),
        LogicalOperatorKind::WorkingTable { working, .. } => Box::new(PhysicalWorkingTable { types, working }),
        LogicalOperatorKind::DelimGet { delim, .. } => Box::new(PhysicalWorkingTable { types, working: delim }),
        LogicalOperatorKind::DelimJoin {
            join_type,
            conditions,
            predicate,
            delim,
            error_on_multiple_rows,
            ..
        } => Box::new(special_joins::PhysicalDelimJoin::new(
            types,
            join_type,
            conditions,
            predicate,
            delim,
            error_on_multiple_rows,
            children,
        )),
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
        LogicalOperatorKind::ColumnSelect { .. } => unreachable!("create_column_select"),
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
        LogicalOperatorKind::AsOfJoin {
            join_type,
            conditions,
            left,
            op,
            right,
            predicate,
        } => Box::new(special_joins::PhysicalAsOfJoin {
            types,
            join_type,
            conditions,
            inequality: (left, op, right),
            predicate,
            children,
            build: Mutex::default(),
        }),
        LogicalOperatorKind::PositionalJoin => Box::new(special_joins::PhysicalPositionalJoin {
            types,
            children,
            rows: Mutex::default(),
            position: Mutex::default(),
        }),
        LogicalOperatorKind::MarkJoin {
            conditions, predicate, ..
        } if !conditions.is_empty() => Box::new(join::PhysicalJoin::new(
            types,
            duckrs_parser::ast::JoinType::Mark,
            conditions,
            predicate,
            children,
        )),
        LogicalOperatorKind::MarkJoin { predicate, .. } => Box::new(special_joins::PhysicalMarkJoin {
            types,
            predicate,
            children,
            rows: Mutex::default(),
        }),
        LogicalOperatorKind::Insert {
            table,
            column_index_map,
            returning,
            on_conflict,
            ..
        } => {
            let empty = || Mutex::new(ColumnDataCollection::new(table.types()));
            let (pending, output) = (empty(), empty());
            Box::new(PhysicalInsert {
                types,
                column_index_map,
                children: std::mem::take(&mut children),
                inserted: Mutex::new(0),
                first_new_row: Mutex::new(None),
                returning,
                on_conflict: on_conflict.map(|action| *action),
                pending,
                output,
                table,
            })
        }
        LogicalOperatorKind::CreateTable {
            schema,
            name,
            columns,
            generated,
            constraints,
            on_conflict,
        } => Box::new(PhysicalCreateTable {
            types,
            schema,
            table_name: name,
            columns,
            generated,
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
    /// the last pipeline of this input of the sink
    finish_input: bool,
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
            // A finished operator wants no more input, which only stops what feeds it: the
            // operators above it still emit the output of the rows it already passed on.
            let finished = self.operators[index + 1..].iter().any(|op| op.is_finished());
            if result == OperatorResult::NeedMoreInput || finished {
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
        if self.finish_input {
            self.sink.finish_input(self.input)?;
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
                finish_input: false,
                finalize: false,
            })
            .collect();
        pipelines.push(Pipeline {
            source: current,
            operators,
            sink,
            input,
            finish_input: false,
            finalize: false,
        });
        pipelines.extend(unmatched);
        pipelines.last_mut().expect("just added").finish_input = true;
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
