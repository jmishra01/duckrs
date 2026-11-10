// Ported from duckdb/src/main/query_profiler.cpp and duckdb/src/main/profiler/gathered_metrics.cpp
// @ rust-port-baseline
//
//! The query profiler: what each operator of a query did, gathered while the pipelines run, and
//! the metrics of the query it adds up to.

use super::physical::PhysicalOperator;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `OperatorMetrics`: what an operator did.
#[derive(Debug, Clone, Copy, Default)]
pub struct OperatorMetrics {
    pub time: Duration,
    /// the rows it gave on (`elements_returned`)
    pub rows: u64,
    /// the rows a scan read
    pub rows_scanned: u64,
}

/// What the operators of the query running on this thread did, by the address of the operator.
#[derive(Debug, Default)]
pub struct OperatorProfiler {
    operators: parking_lot::Mutex<HashMap<usize, OperatorMetrics>>,
}

fn key(op: &dyn PhysicalOperator) -> usize {
    std::ptr::from_ref(op).cast::<()>() as usize
}

impl OperatorProfiler {
    /// `OperatorProfiler::EndOperator`: `op` took `time` and gave on `rows`.
    pub fn record(&self, op: &dyn PhysicalOperator, time: Duration, rows: usize, scanned: bool) {
        let mut operators = self.operators.lock();
        let metrics = operators.entry(key(op)).or_default();
        metrics.time += time;
        metrics.rows += rows as u64;
        if scanned {
            metrics.rows_scanned += rows as u64;
        }
    }

    /// `op` took `time`, without giving on rows (a sink taking its input).
    pub fn add_time(&self, op: &dyn PhysicalOperator, time: Duration) {
        self.operators.lock().entry(key(op)).or_default().time += time;
    }

    pub fn metrics(&self, op: &dyn PhysicalOperator) -> OperatorMetrics {
        self.operators.lock().get(&key(op)).copied().unwrap_or_default()
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<OperatorProfiler>>> = const { RefCell::new(None) };
}

/// The profiler of the query running on this thread, if it is profiled.
pub fn current() -> Option<Arc<OperatorProfiler>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Runs `body` with `profiler` gathering what the operators do on this thread.
pub fn with_profiler<T>(profiler: Option<Arc<OperatorProfiler>>, body: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| std::mem::replace(&mut *current.borrow_mut(), profiler));
    let result = body();
    CURRENT.with(|current| *current.borrow_mut() = previous);
    result
}

/// Times `body` for `op`, if a profiler is on.
pub fn timed<T>(profiler: Option<&OperatorProfiler>, body: impl FnOnce() -> T) -> (T, Duration) {
    if profiler.is_none() {
        return (body(), Duration::ZERO);
    }
    let start = Instant::now();
    let result = body();
    (result, start.elapsed())
}

/// Whether `op` reads rows from storage or a file, which then count as scanned.
pub fn is_scan(op: &dyn PhysicalOperator) -> bool {
    matches!(op.name(), "TABLE_SCAN" | "READ_CSV" | "READ_JSON" | "READ_PARQUET")
}

/// `PhysicalOperatorType` of an operator: the table functions are TABLE_SCANs.
pub fn operator_type(op: &dyn PhysicalOperator) -> &'static str {
    match op.name() {
        "READ_CSV" | "READ_JSON" | "READ_PARQUET" | "RANGE" => "TABLE_SCAN",
        name => name,
    }
}

/// `PhysicalOperator::GetName`: a table scan is a SEQ_SCAN, a table function its name.
pub fn operator_name(op: &dyn PhysicalOperator) -> &'static str {
    match op.name() {
        "TABLE_SCAN" => "SEQ_SCAN",
        name => name,
    }
}

/// `QueryProfiler::OperatorRequiresProfiling` with `profiling_coverage = 'SELECT'`.
pub fn select_operator(op_type: &str) -> bool {
    matches!(
        op_type,
        "ORDER_BY"
            | "RESERVOIR_SAMPLE"
            | "STREAMING_SAMPLE"
            | "LIMIT"
            | "LIMIT_PERCENT"
            | "STREAMING_LIMIT"
            | "TOP_N"
            | "WINDOW"
            | "UNNEST"
            | "UNGROUPED_AGGREGATE"
            | "HASH_GROUP_BY"
            | "FILTER"
            | "PROJECTION"
            | "COPY_TO_FILE"
            | "TABLE_SCAN"
            | "CHUNK_SCAN"
            | "DELIM_SCAN"
            | "EXPRESSION_SCAN"
            | "BLOCKWISE_NL_JOIN"
            | "NESTED_LOOP_JOIN"
            | "HASH_JOIN"
            | "CROSS_PRODUCT"
            | "PIECEWISE_MERGE_JOIN"
            | "IE_JOIN"
            | "LEFT_DELIM_JOIN"
            | "RIGHT_DELIM_JOIN"
            | "UNION"
            | "CTE"
            | "CTE_SCAN"
            | "RECURSIVE_CTE"
            | "RECURSIVE_KEY_CTE"
            | "RECURSIVE_KEY_JOIN"
            | "EMPTY_RESULT"
            | "EXTENSION"
    )
}

// ---------------------------------------------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------------------------------------------

/// A value of a metric.
#[derive(Debug, Clone)]
pub enum Metric {
    Double(f64),
    UBigInt(u64),
    Text(String),
    /// `extra_info`: names to their text
    Map(Vec<(String, String)>),
}

/// `ProfilingNode`: an operator of the profile, and the ones it reads.
#[derive(Debug, Clone)]
pub struct ProfileNode {
    pub op_type: String,
    pub name: String,
    pub extra_info: Vec<(String, String)>,
    pub metrics: OperatorMetrics,
    pub children: Vec<ProfileNode>,
}

impl ProfileNode {
    /// The tree of `op`, with what the profiler saw of each operator. With `select_coverage`, the
    /// operators C++ doesn't profile then are left out, their children taking their place.
    pub fn of(op: &dyn PhysicalOperator, profiler: &OperatorProfiler, select_coverage: bool) -> Vec<ProfileNode> {
        let children: Vec<ProfileNode> = op
            .children()
            .iter()
            .flat_map(|child| Self::of(child.as_ref(), profiler, select_coverage))
            .collect();
        let op_type = operator_type(op);
        if op.name() == "COLUMN_SELECT" || (select_coverage && !select_operator(op_type)) {
            return children;
        }
        let extra_info = op
            .explain_details()
            .into_iter()
            .enumerate()
            .map(|(i, detail)| match detail.split_once(": ") {
                Some((key, value)) => (key.to_string(), value.to_string()),
                None => (format!("{i}"), detail),
            })
            .collect();
        vec![ProfileNode {
            op_type: op_type.to_string(),
            name: operator_name(op).to_string(),
            extra_info,
            metrics: profiler.metrics(op),
            children,
        }]
    }

    /// `MergeOperatorMeasurements`: the sums over the tree.
    pub fn totals(&self) -> OperatorMetrics {
        let mut total = self.metrics;
        for child in &self.children {
            let sub = child.totals();
            total.time += sub.time;
            total.rows += sub.rows;
            total.rows_scanned += sub.rows_scanned;
        }
        total
    }
}

/// `GatheredMetrics`: which metrics `tracked_metrics` asks for.
#[derive(Debug, Clone)]
pub struct TrackedMetrics {
    patterns: Vec<String>,
}

impl TrackedMetrics {
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    /// `MetricIsTracked`: `*`, an exact name, a `prefix*`, or another glob.
    pub fn is_tracked(&self, key: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            pattern == "*" || pattern == key || super::file_system::glob_match(key.as_bytes(), pattern.as_bytes())
        })
    }

    /// Whether any metric of the operators is tracked (`AnyOperatorMetricTracked`).
    pub fn any_operator_metric(&self) -> bool {
        [
            "type",
            "name",
            "timing",
            "cpu_time",
            "intermediate_rows",
            "rows_scanned",
            "extra_info",
        ]
        .iter()
        .any(|metric| self.is_tracked(&format!("operator.{metric}")))
    }
}

/// A query's profile: its metrics by their dotted names, and its operators.
#[derive(Debug, Clone)]
pub struct QueryProfile {
    pub metrics: BTreeMap<String, Metric>,
    pub root: Option<ProfileNode>,
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn metric_json(metric: &Metric) -> String {
    match metric {
        Metric::Double(value) if value.is_finite() => format!("{value}"),
        Metric::Double(_) => "null".to_string(),
        Metric::UBigInt(value) => value.to_string(),
        Metric::Text(text) => json_string(text),
        Metric::Map(entries) => {
            let fields: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", json_string(key), json_string(value)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
    }
}

/// A nested object of metrics, as `AddMetricToResult` builds it from the dotted names.
#[derive(Default)]
struct JsonObject {
    entries: Vec<(String, JsonEntry)>,
}

enum JsonEntry {
    Value(String),
    Object(JsonObject),
}

impl JsonObject {
    fn add(&mut self, key: &str, value: String) {
        match key.split_once('.') {
            None => self.entries.push((key.to_string(), JsonEntry::Value(value))),
            Some((prefix, rest)) => {
                let position = self
                    .entries
                    .iter()
                    .position(|(k, entry)| k == prefix && matches!(entry, JsonEntry::Object(_)));
                let index = match position {
                    Some(index) => index,
                    None => {
                        self.entries
                            .push((prefix.to_string(), JsonEntry::Object(JsonObject::default())));
                        self.entries.len() - 1
                    }
                };
                if let JsonEntry::Object(object) = &mut self.entries[index].1 {
                    object.add(rest, value);
                }
            }
        }
    }

    fn render(&self, indent: usize, out: &mut String) {
        out.push_str("{\n");
        for (i, (key, entry)) in self.entries.iter().enumerate() {
            out.push_str(&"    ".repeat(indent + 1));
            out.push_str(&json_string(key));
            out.push_str(": ");
            match entry {
                JsonEntry::Value(value) => out.push_str(value),
                JsonEntry::Object(object) => object.render(indent + 1, out),
            }
            if i + 1 < self.entries.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str(&"    ".repeat(indent));
        out.push('}');
    }
}

fn seconds(duration: Duration) -> Metric {
    Metric::Double(duration.as_secs_f64())
}

/// `OperatorToResultTree`: an operator's tracked metrics, and its children.
fn operator_json(node: &ProfileNode, tracked: &TrackedMetrics, legacy: bool) -> JsonObject {
    let mut object = JsonObject::default();
    let metrics = [
        ("type", Metric::Text(node.op_type.clone()), "operator_type"),
        ("name", Metric::Text(node.name.clone()), "operator_name"),
        ("timing", seconds(node.metrics.time), "operator_timing"),
        ("cpu_time", seconds(node.metrics.time), ""),
        (
            "intermediate_rows",
            Metric::UBigInt(node.metrics.rows),
            "operator_cardinality",
        ),
        (
            "rows_scanned",
            Metric::UBigInt(node.metrics.rows_scanned),
            "operator_rows_scanned",
        ),
        ("extra_info", Metric::Map(node.extra_info.clone()), "extra_info"),
    ];
    for (name, value, legacy_name) in metrics {
        if !tracked.is_tracked(&format!("operator.{name}")) {
            continue;
        }
        if legacy {
            if !legacy_name.is_empty() {
                object
                    .entries
                    .push((legacy_name.to_string(), JsonEntry::Value(metric_json(&value))));
            }
        } else {
            object
                .entries
                .push((name.to_string(), JsonEntry::Value(metric_json(&value))));
        }
    }
    if legacy {
        let totals = node.totals();
        for (name, value) in [
            ("cpu_time", seconds(totals.time)),
            ("cumulative_cardinality", Metric::UBigInt(totals.rows)),
            ("cumulative_rows_scanned", Metric::UBigInt(totals.rows_scanned)),
        ] {
            object
                .entries
                .push((name.to_string(), JsonEntry::Value(metric_json(&value))));
        }
    }
    let children: Vec<String> = node
        .children
        .iter()
        .map(|child| {
            let mut out = String::new();
            operator_json(child, tracked, legacy).render(0, &mut out);
            out
        })
        .collect();
    if !children.is_empty() {
        object.entries.push((
            "children".to_string(),
            JsonEntry::Value(format!("[{}]", children.join(", "))),
        ));
    }
    object
}

/// The names C++'s legacy format gives the query's metrics (`ToLegacyResultTree`).
const LEGACY_NAMES: [(&str, &str); 14] = [
    ("total_bytes_written", "io.total_bytes_written"),
    ("total_write_operations", "io.total_write_operations"),
    ("total_bytes_read", "io.total_bytes_read"),
    ("total_read_operations", "io.total_read_operations"),
    ("result_set_size", "query.total_intermediate_size_bytes"),
    ("latency", "query.total_time"),
    ("commit_local_storage_latency", "storage.commit_local_storage_latency"),
    ("query_name", "query.sql"),
    ("cpu_time", "query.cpu_time"),
    ("checkpoint_latency", "storage.checkpoint_latency"),
    ("cumulative_cardinality", "query.total_intermediate_rows"),
    ("write_to_wal_latency", "storage.write_to_wal_latency"),
    ("blocked_thread_time", "system.blocked_thread_time"),
    ("cumulative_rows_scanned", "query.total_rows_scanned"),
];

impl QueryProfile {
    /// `QueryProfiler::ToJSON` of `ToResultTree`, or of `ToLegacyResultTree`.
    pub fn to_json(&self, tracked: &TrackedMetrics, legacy: bool) -> String {
        let mut object = JsonObject::default();
        if self.root.is_none() && !self.metrics.contains_key("query.sql") {
            object
                .entries
                .push(("result".to_string(), JsonEntry::Value(json_string("empty"))));
        } else if legacy {
            for (legacy_name, name) in LEGACY_NAMES {
                if let Some(value) = self.metrics.get(name) {
                    object
                        .entries
                        .push((legacy_name.to_string(), JsonEntry::Value(metric_json(value))));
                }
            }
            if let Some(root) = &self.root {
                object.entries.push((
                    "rows_returned".to_string(),
                    JsonEntry::Value(root.metrics.rows.to_string()),
                ));
                let mut out = String::new();
                operator_json(root, tracked, true).render(1, &mut out);
                object
                    .entries
                    .push(("children".to_string(), JsonEntry::Value(format!("[{out}]"))));
            }
        } else {
            for (name, value) in &self.metrics {
                if tracked.is_tracked(name) {
                    object.add(name, metric_json(value));
                }
            }
            if let Some(root) = &self.root
                && tracked.any_operator_metric()
            {
                let mut out = String::new();
                operator_json(root, tracked, false).render(1, &mut out);
                object
                    .entries
                    .push(("operator".to_string(), JsonEntry::Value(format!("[{out}]"))));
            }
        }
        let mut out = String::new();
        object.render(0, &mut out);
        out
    }
}
