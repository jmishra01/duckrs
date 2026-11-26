//! User-defined functions through the Rust API (`duckrs::udf`): scalar, aggregate and table functions that the
//! engine calls on whole chunks.

use duckrs::udf::*;
use duckrs::{Database, DataChunk, LogicalType, Result, Value};
use duckrs_common::vector::Vector;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// A volatile counter: each row gets the next number, from 5 on, for each bound call.
struct Counter;

struct CounterRun(AtomicI64);

impl ScalarUdf for Counter {
    fn bind(&self, _: &ScalarUdfBindInput<'_>) -> Result<Arc<dyn ScalarUdfExecutor>> {
        Ok(Arc::new(CounterRun(AtomicI64::new(5))))
    }
}

impl ScalarUdfExecutor for CounterRun {
    fn execute(&self, arguments: &DataChunk, result: &mut Vector) -> Result<()> {
        let data = result.data_mut::<i64>();
        for slot in data.iter_mut().take(arguments.len()) {
            *slot = self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn scalar_set(name: &str, function: Arc<dyn ScalarUdf>, volatile: bool) -> ScalarUdfSet {
    ScalarUdfSet {
        name: name.to_string(),
        functions: vec![ScalarUdfOverload {
            name: name.to_string(),
            arguments: vec![LogicalType::BIGINT],
            varargs: None,
            return_type: LogicalType::BIGINT,
            special_handling: false,
            volatile,
            function,
        }],
    }
}

fn column(result: &duckrs::QueryResult, column: usize) -> Vec<Value> {
    (0..result.row_count()).map(|row| result.value(column, row)).collect()
}

#[test]
fn volatile_scalar_function_runs_per_row_in_order() {
    let db = Database::open_in_memory().unwrap();
    let mut con = db.connect().unwrap();
    con.register_scalar_functions(scalar_set("my_counter", Arc::new(Counter), true)).unwrap();
    let result = con.query("SELECT my_counter(i) FROM range(3) r(i)").unwrap();
    assert_eq!(column(&result, 0), vec![Value::bigint(5), Value::bigint(6), Value::bigint(7)]);
    let result = con.query("SELECT my_counter(0) FROM range(3)").unwrap();
    assert_eq!(column(&result, 0), vec![Value::bigint(5), Value::bigint(6), Value::bigint(7)]);
}

/// `sum` of BIGINTs as a user-defined aggregate.
struct Sum;

impl AggregateUdf for Sum {
    fn new_state(&self) -> Result<UdfState> {
        Ok(Box::new(None::<i64>))
    }
    fn update(&self, input: &DataChunk, states: &mut [&mut UdfState], rows: &[usize]) -> Result<()> {
        let column = input.column(0);
        for (row, &state) in rows.iter().enumerate() {
            if let Some(value) = column.get_value(row).as_i128() {
                let sum = states[state].downcast_mut::<Option<i64>>().unwrap();
                *sum = Some(sum.unwrap_or(0) + value as i64);
            }
        }
        Ok(())
    }
    fn combine(&self, sources: &mut [&mut UdfState], targets: &mut [&mut UdfState]) -> Result<()> {
        for (source, target) in sources.iter_mut().zip(targets.iter_mut()) {
            let source = *source.downcast_mut::<Option<i64>>().unwrap();
            let target = target.downcast_mut::<Option<i64>>().unwrap();
            if let Some(source) = source {
                *target = Some(target.unwrap_or(0) + source);
            }
        }
        Ok(())
    }
    fn finalize(&self, states: &mut [&mut UdfState], result: &mut Vector) -> Result<()> {
        for (row, state) in states.iter_mut().enumerate() {
            let value = match state.downcast_mut::<Option<i64>>().unwrap() {
                Some(sum) => Value::bigint(*sum),
                None => Value::null(LogicalType::BIGINT),
            };
            result.set_value(row, &value)?;
        }
        Ok(())
    }
}

#[test]
fn aggregate_function_groups_and_windows() {
    let db = Database::open_in_memory().unwrap();
    let mut con = db.connect().unwrap();
    let function = user_aggregate_function("my_sum", vec![LogicalType::BIGINT], LogicalType::BIGINT, Arc::new(Sum));
    con.register_aggregate_functions(user_aggregate_set("my_sum", vec![function])).unwrap();
    let result = con
        .query("SELECT i % 2 AS g, my_sum(i) FROM range(10) t(i) GROUP BY g ORDER BY g")
        .unwrap();
    assert_eq!(column(&result, 1), vec![Value::bigint(20), Value::bigint(25)]);
    let result = con.query("SELECT my_sum(i) FROM range(0) t(i)").unwrap();
    assert_eq!(column(&result, 0), vec![Value::null(LogicalType::BIGINT)]);
    let result = con.query("SELECT my_sum(i) OVER () FROM range(4) t(i) LIMIT 1").unwrap();
    assert_eq!(column(&result, 0), vec![Value::bigint(6)]);
}

/// `count_to(n)`: the numbers 0 to n - 1, in chunks of at most 1000.
struct CountTo;

struct CountToScan(i64);

struct CountToRun {
    next: i64,
    end: i64,
}

impl TableUdf for CountTo {
    fn bind(&self, input: TableUdfBindInput) -> Result<TableUdfBinding> {
        let end = input.parameters[0].as_i128().unwrap_or(0) as i64;
        Ok(TableUdfBinding {
            names: vec!["n".to_string()],
            types: vec![LogicalType::BIGINT],
            cardinality: Some((end as u64, true)),
            scan: Arc::new(CountToScan(end)),
        })
    }
}

impl TableUdfScan for CountToScan {
    fn init(&self, _: &[usize]) -> Result<Box<dyn TableUdfRun>> {
        Ok(Box::new(CountToRun { next: 0, end: self.0 }))
    }
}

impl TableUdfRun for CountToRun {
    fn next(&mut self, output: &mut DataChunk) -> Result<()> {
        let count = (self.end - self.next).clamp(0, 1000) as usize;
        let data = output.column_mut(0).data_mut::<i64>();
        for (i, slot) in data.iter_mut().take(count).enumerate() {
            *slot = self.next + i as i64;
        }
        self.next += count as i64;
        output.set_len(count);
        Ok(())
    }
}

#[test]
fn table_function_gives_its_chunks() {
    let db = Database::open_in_memory().unwrap();
    let mut con = db.connect().unwrap();
    let set = TableUdfSet {
        name: "count_to".to_string(),
        functions: vec![Arc::new(TableUdfOverload {
            name: "count_to".to_string(),
            arguments: vec![LogicalType::BIGINT],
            named_parameters: Vec::new(),
            projection_pushdown: false,
            function: Arc::new(CountTo),
        })],
    };
    con.register_table_functions(set).unwrap();
    let result = con.query("SELECT count(*), sum(n) FROM count_to(2500)").unwrap();
    assert_eq!(result.value(0, 0), Value::bigint(2500));
    let error = con.query("SELECT * FROM count_to(1, x := 2)").unwrap_err();
    assert!(error.message.contains("Invalid named parameter \"x\""), "{}", error.message);
}
