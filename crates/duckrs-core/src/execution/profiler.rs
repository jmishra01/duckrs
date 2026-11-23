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
    /// `DataChunk::GetDataSize` of what it gave on
    pub size_bytes: u64,
    /// the bytes of its files a scan read (`operator.bytes_scanned`)
    pub bytes_scanned: u64,
}

/// `Vector::GetDataSize`, near enough: the width of each row's slot (16 for strings, the
/// `string_t`), and of the children of nested types.
fn type_size(ty: &duckrs_common::types::LogicalType) -> u64 {
    use duckrs_common::types::PhysicalType as P;
    match ty.physical_type() {
        P::Bool | P::Int8 | P::UInt8 => 1,
        P::Int16 | P::UInt16 => 2,
        P::Int32 | P::UInt32 | P::Float => 4,
        P::Int64 | P::UInt64 | P::Double => 8,
        P::Int128 | P::UInt128 | P::Interval | P::Varchar | P::List => 16,
        P::Struct => ty.struct_children().iter().map(|(_, child)| type_size(child)).sum(),
        P::Array => {
            let child = ty.child_type().map_or(0, type_size);
            child * u64::from(ty.array_size().unwrap_or(0))
        }
        _ => 8,
    }
}

fn chunk_size(chunk: &duckrs_common::vector::DataChunk) -> u64 {
    let rows = chunk.len() as u64;
    chunk.types().iter().map(|ty| rows * type_size(ty)).sum()
}

/// What the operators of the query running on this thread did, by the address of the operator,
/// and the counters and timings of what it did outside of them (I/O, storage), by metric name.
#[derive(Debug, Default)]
pub struct OperatorProfiler {
    operators: parking_lot::Mutex<HashMap<usize, OperatorMetrics>>,
    counters: parking_lot::Mutex<BTreeMap<&'static str, u64>>,
    timings: parking_lot::Mutex<BTreeMap<&'static str, Duration>>,
}

/// `QueryProfiler::AddToMetricCounter` on the profiler of this thread, if there is one.
pub fn add_counter(name: &'static str, amount: u64) {
    if let Some(profiler) = current() {
        *profiler.counters.lock().entry(name).or_default() += amount;
    }
}

/// `QueryProfiler::AddToMetricTiming` on the profiler of this thread, if there is one.
pub fn add_timing(name: &'static str, time: Duration) {
    if let Some(profiler) = current() {
        *profiler.timings.lock().entry(name).or_default() += time;
    }
}

/// `TrackBytesRead`: one read of `bytes` bytes.
pub fn track_read(bytes: usize) {
    add_counter("io.total_bytes_read", bytes as u64);
    add_counter("io.total_read_operations", 1);
}

/// `TrackBytesWritten`: one write of `bytes` bytes.
pub fn track_write(bytes: usize) {
    add_counter("io.total_bytes_written", bytes as u64);
    add_counter("io.total_write_operations", 1);
}

/// Times `body` as the metric `name`.
pub fn time_metric<T>(name: &'static str, body: impl FnOnce() -> T) -> T {
    if current().is_none() {
        return body();
    }
    let start = Instant::now();
    let result = body();
    add_timing(name, start.elapsed());
    result
}

pub fn key(op: &dyn PhysicalOperator) -> usize {
    std::ptr::from_ref(op).cast::<()>() as usize
}

impl OperatorProfiler {
    /// `OperatorProfiler::EndOperator`: `op` took `time` and gave on `rows`.
    pub fn record(
        &self,
        op: &dyn PhysicalOperator,
        time: Duration,
        chunk: &duckrs_common::vector::DataChunk,
        scanned: bool,
    ) {
        let rows = chunk.len() as u64;
        let size = chunk_size(chunk);
        let mut operators = self.operators.lock();
        let metrics = operators.entry(key(op)).or_default();
        metrics.time += time;
        metrics.rows += rows;
        metrics.size_bytes += size;
        if scanned {
            metrics.rows_scanned += rows;
        }
    }

    /// The rows a scan that counts them read, in place of those it gave on (`TableScanState::rows_scanned`).
    pub fn set_rows_scanned(&self, op: &dyn PhysicalOperator) {
        if let Some(rows) = op.rows_read().filter(|_| is_scan(op)) {
            self.operators.lock().entry(key(op)).or_default().rows_scanned = rows;
        }
    }

    /// A scan read `bytes` of its files (`OperatorProfiler::AddBytesScanned`).
    pub fn add_bytes_scanned(&self, op: &dyn PhysicalOperator, bytes: u64) {
        self.operators.lock().entry(key(op)).or_default().bytes_scanned += bytes;
    }

    /// `op` took `time`, without giving on rows (a sink taking its input).
    pub fn add_time(&self, op: &dyn PhysicalOperator, time: Duration) {
        self.operators.lock().entry(key(op)).or_default().time += time;
    }

    /// Forgets what operators did so far: those of a plan that is gone can share addresses with
    /// the next one.
    pub fn clear_operators(&self) {
        self.operators.lock().clear();
    }

    /// The counters and timings it gathered.
    pub fn gathered(&self) -> (BTreeMap<&'static str, u64>, BTreeMap<&'static str, Duration>) {
        (self.counters.lock().clone(), self.timings.lock().clone())
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
            | "PERFECT_HASH_GROUP_BY"
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
            | "REC_CTE"
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
    /// of a table scan: the row groups it scanned, of those it could have
    pub row_groups: (u64, u64),
    /// of a scan that counts the bytes it reads, those it read (`bytes_scanned_reported`)
    pub bytes_scanned: Option<u64>,
    pub children: Vec<ProfileNode>,
}

impl ProfileNode {
    /// The tree of `op`, with what the profiler saw of each operator. With `select_coverage`, the
    /// operators C++ doesn't profile then are left out, their children taking their place.
    pub fn of(op: &dyn PhysicalOperator, profiler: &OperatorProfiler, select_coverage: bool) -> Vec<ProfileNode> {
        Self::of_with(op, profiler, select_coverage, None)
    }

    /// [`ProfileNode::of`], with the details EXPLAIN shows of each operator, by its address, as its `extra_info`.
    pub fn of_with(
        op: &dyn PhysicalOperator,
        profiler: &OperatorProfiler,
        select_coverage: bool,
        infos: Option<&std::collections::HashMap<usize, Vec<(String, String)>>>,
    ) -> Vec<ProfileNode> {
        let details = |op: &dyn PhysicalOperator| -> Vec<(String, String)> {
            if let Some(info) = infos.and_then(|infos| infos.get(&key(op))) {
                return info.iter().filter(|(_, value)| !value.is_empty()).cloned().collect();
            }
            op.explain_details()
                .into_iter()
                .enumerate()
                .map(|(i, detail)| match detail.split_once(": ") {
                    Some((key, value)) => (key.to_string(), value.to_string()),
                    None => (format!("{i}"), detail),
                })
                .collect()
        };
        // `CollapseSecureViews`: of a secure view only the boundary is left, with the time of all that is inside it;
        // the other metrics of what is inside describe the view's contents and are dropped
        if op.name() == "SECURE_VIEW" {
            fn subtree_time(op: &dyn PhysicalOperator, profiler: &OperatorProfiler) -> Duration {
                op.children()
                    .iter()
                    .map(|child| profiler.metrics(child.as_ref()).time + subtree_time(child.as_ref(), profiler))
                    .sum()
            }
            let mut metrics = profiler.metrics(op);
            metrics.time += subtree_time(op, profiler);
            return vec![ProfileNode {
                op_type: operator_type(op).to_string(),
                name: operator_name(op).to_string(),
                extra_info: details(op),
                metrics,
                row_groups: (0, 0),
                bytes_scanned: None,
                children: Vec::new(),
            }];
        }
        let children: Vec<ProfileNode> = op
            .children()
            .iter()
            .flat_map(|child| Self::of_with(child.as_ref(), profiler, select_coverage, infos))
            .collect();
        let op_type = operator_type(op);
        if op.name() == "COLUMN_SELECT" || (select_coverage && !select_operator(op_type)) {
            return children;
        }
        let extra_info = details(op);
        vec![ProfileNode {
            op_type: op_type.to_string(),
            name: operator_name(op).to_string(),
            extra_info,
            metrics: profiler.metrics(op),
            row_groups: op
                .row_groups_scanned()
                .map_or((0, 0), |(scanned, total)| (scanned as u64, total as u64)),
            bytes_scanned: op.reports_bytes_scanned().then(|| profiler.metrics(op).bytes_scanned),
            children,
        }]
    }

    /// `query.total_bytes_scanned`: the bytes the scans of the tree read, unless one of them doesn't count them
    /// (`HasScanWithoutBytesScanned`).
    pub fn total_bytes_scanned(&self) -> Option<u64> {
        if self.op_type == "TABLE_SCAN" && self.bytes_scanned.is_none() {
            return None;
        }
        let mut total = self.bytes_scanned.unwrap_or(0);
        for child in &self.children {
            total += child.total_bytes_scanned()?;
        }
        Some(total)
    }

    /// `MergeOperatorMeasurements`: the sums over the tree.
    pub fn totals(&self) -> OperatorMetrics {
        let mut total = self.metrics;
        for child in &self.children {
            let sub = child.totals();
            total.time += sub.time;
            total.rows += sub.rows;
            total.rows_scanned += sub.rows_scanned;
            total.size_bytes += sub.size_bytes;
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
            "intermediate_rows",
            "intermediate_size_bytes",
            "rows_scanned",
            "row_groups_scanned",
            "total_row_groups_to_scan",
            "bytes_scanned",
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
    let scan = node.op_type == "TABLE_SCAN";
    let mut metrics = vec![
        ("type", Metric::Text(node.op_type.clone()), "operator_type"),
        ("name", Metric::Text(node.name.clone()), "operator_name"),
        ("timing", seconds(node.metrics.time), "operator_timing"),
        (
            "intermediate_rows",
            Metric::UBigInt(node.metrics.rows),
            "operator_cardinality",
        ),
        (
            "intermediate_size_bytes",
            Metric::UBigInt(node.metrics.size_bytes),
            "result_set_size",
        ),
    ];
    // `OperatorMetrics::GetMetrics`: what only a table scan has
    if scan {
        metrics.push((
            "rows_scanned",
            Metric::UBigInt(node.metrics.rows_scanned),
            "operator_rows_scanned",
        ));
        metrics.push(("row_groups_scanned", Metric::UBigInt(node.row_groups.0), ""));
        metrics.push(("total_row_groups_to_scan", Metric::UBigInt(node.row_groups.1), ""));
        if let Some(bytes) = node.bytes_scanned {
            metrics.push(("bytes_scanned", Metric::UBigInt(bytes), ""));
        }
    }
    metrics.push(("extra_info", Metric::Map(node.extra_info.clone()), "extra_info"));
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
    // `QueryProfileResultToJSON`: the keys in order, the values before what nests
    let nested = |(key, entry): &(String, JsonEntry)| match entry {
        JsonEntry::Object(_) => true,
        JsonEntry::Value(text) => key == "children" || text.starts_with('{') || text.starts_with('['),
    };
    object.entries.sort_by(|a, b| nested(a).cmp(&nested(b)).then_with(|| a.0.cmp(&b.0)));
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
