// Ported from duckdb/extension/tpch/dbgen/dbgen.cpp @ rust-port-baseline
//
// The DuckDB side of dbgen: the TPC-H schema, the row generators that append straight into data
// chunks, the generator that walks the tables (in steps, skipping the seeds ahead), and the
// queries and answers.
//
// Differences from the C++, which appends into catalog tables:
// - the generator yields the chunks (tagged with their table) instead of appending them, in the
//   order the C++ hands them to its appenders in sequential mode (`DBGenMode::SEQUENTIAL`, which
//   the C++ uses with one thread or an explicit `children`/`step`). The C++ parallel mode
//   generates the same rows per table in the same order, only cut into chunks differently;
// - strings are copied into the chunks' string buffers rather than referenced in the text pool;
// - progress counts generated rows as they are generated, rather than per batch of 100000.

use crate::bm_utils::{Distribution, pick_distribution, unjulian};
use crate::build::mk_sparse;
use crate::constants::{TPCH_ANSWERS_SF0_01, TPCH_ANSWERS_SF0_1, TPCH_ANSWERS_SF1, TPCH_QUERIES, TPCH_QUERIES_COUNT};
use crate::dbgen_gunk::{Dists, load_dists};
use crate::dss::*;
use crate::permute::permute_dist;
use crate::rnd::{dss_random, row_start, row_stop_h};
use crate::rng64::dss_random64;
use crate::speed_seed::{sd_cust, sd_line, sd_nation, sd_order, sd_part, sd_psupp, sd_region, sd_supp};
use crate::text::TextPool;
use duckrs_common::types::{Date, LogicalType};
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

/// `RANDOM(tgt, lower, upper, seed)`
#[inline]
fn random(lower: DssHuge, upper: DssHuge, seed: &mut Seed) -> DssHuge {
    dss_random(lower, upper, seed)
}

/// `int(average_length * V_STR_LOW)`, `int(average_length * V_STR_HGH)`
fn v_str_bounds(average_length: i32) -> (i32, i32) {
    (
        (f64::from(average_length) * V_STR_LOW) as i32,
        (f64::from(average_length) * V_STR_HGH) as i32,
    )
}

// ---------------------------------------------------------------------------------------------
// The schema
// ---------------------------------------------------------------------------------------------

/// A TPC-H table, as dbgen generates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TpchTable {
    Part,
    PartSupp,
    Supplier,
    Customer,
    Orders,
    Lineitem,
    Nation,
    Region,
}

/// The type of a TPC-H column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Integer,
    BigInt,
    /// `DECIMAL(15, 2)`
    Decimal,
    Varchar,
    Date,
}

impl ColumnKind {
    fn logical_type(self) -> LogicalType {
        match self {
            ColumnKind::Integer => LogicalType::INTEGER,
            ColumnKind::BigInt => LogicalType::BIGINT,
            ColumnKind::Decimal => LogicalType::decimal(15, 2),
            ColumnKind::Varchar => LogicalType::VARCHAR,
            ColumnKind::Date => LogicalType::DATE,
        }
    }
}

use ColumnKind::{BigInt as B, Date as D, Decimal as M, Integer as I, Varchar as V};

const REGION_COLUMNS: &[(&str, ColumnKind)] = &[("r_regionkey", I), ("r_name", V), ("r_comment", V)];
const NATION_COLUMNS: &[(&str, ColumnKind)] =
    &[("n_nationkey", I), ("n_name", V), ("n_regionkey", I), ("n_comment", V)];
const SUPPLIER_COLUMNS: &[(&str, ColumnKind)] = &[
    ("s_suppkey", B),
    ("s_name", V),
    ("s_address", V),
    ("s_nationkey", I),
    ("s_phone", V),
    ("s_acctbal", M),
    ("s_comment", V),
];
const CUSTOMER_COLUMNS: &[(&str, ColumnKind)] = &[
    ("c_custkey", B),
    ("c_name", V),
    ("c_address", V),
    ("c_nationkey", I),
    ("c_phone", V),
    ("c_acctbal", M),
    ("c_mktsegment", V),
    ("c_comment", V),
];
const PART_COLUMNS: &[(&str, ColumnKind)] = &[
    ("p_partkey", B),
    ("p_name", V),
    ("p_mfgr", V),
    ("p_brand", V),
    ("p_type", V),
    ("p_size", I),
    ("p_container", V),
    ("p_retailprice", M),
    ("p_comment", V),
];
const PARTSUPP_COLUMNS: &[(&str, ColumnKind)] = &[
    ("ps_partkey", B),
    ("ps_suppkey", B),
    ("ps_availqty", B),
    ("ps_supplycost", M),
    ("ps_comment", V),
];
const ORDERS_COLUMNS: &[(&str, ColumnKind)] = &[
    ("o_orderkey", B),
    ("o_custkey", B),
    ("o_orderstatus", V),
    ("o_totalprice", M),
    ("o_orderdate", D),
    ("o_orderpriority", V),
    ("o_clerk", V),
    ("o_shippriority", I),
    ("o_comment", V),
];
const LINEITEM_COLUMNS: &[(&str, ColumnKind)] = &[
    ("l_orderkey", B),
    ("l_partkey", B),
    ("l_suppkey", B),
    ("l_linenumber", B),
    ("l_quantity", M),
    ("l_extendedprice", M),
    ("l_discount", M),
    ("l_tax", M),
    ("l_returnflag", V),
    ("l_linestatus", V),
    ("l_shipdate", D),
    ("l_commitdate", D),
    ("l_receiptdate", D),
    ("l_shipinstruct", V),
    ("l_shipmode", V),
    ("l_comment", V),
];

impl TpchTable {
    /// The tables in the order `DBGenWrapper::CreateTPCHSchema` creates them.
    pub const ALL: [TpchTable; 8] = [
        TpchTable::Region,
        TpchTable::Nation,
        TpchTable::Supplier,
        TpchTable::Customer,
        TpchTable::Part,
        TpchTable::PartSupp,
        TpchTable::Orders,
        TpchTable::Lineitem,
    ];

    /// The table name (`get_table_name`), without a suffix.
    pub fn name(self) -> &'static str {
        match self {
            TpchTable::Part => "part",
            TpchTable::PartSupp => "partsupp",
            TpchTable::Supplier => "supplier",
            TpchTable::Customer => "customer",
            TpchTable::Orders => "orders",
            TpchTable::Lineitem => "lineitem",
            TpchTable::Nation => "nation",
            TpchTable::Region => "region",
        }
    }

    /// The dbgen table number (`PART` .. `REGION` of dss.h).
    fn index(self) -> usize {
        match self {
            TpchTable::Part => PART,
            TpchTable::PartSupp => PSUPP,
            TpchTable::Supplier => SUPP,
            TpchTable::Customer => CUST,
            TpchTable::Orders => ORDER,
            TpchTable::Lineitem => LINE,
            TpchTable::Nation => NATION,
            TpchTable::Region => REGION,
        }
    }

    fn columns(self) -> &'static [(&'static str, ColumnKind)] {
        match self {
            TpchTable::Part => PART_COLUMNS,
            TpchTable::PartSupp => PARTSUPP_COLUMNS,
            TpchTable::Supplier => SUPPLIER_COLUMNS,
            TpchTable::Customer => CUSTOMER_COLUMNS,
            TpchTable::Orders => ORDERS_COLUMNS,
            TpchTable::Lineitem => LINEITEM_COLUMNS,
            TpchTable::Nation => NATION_COLUMNS,
            TpchTable::Region => REGION_COLUMNS,
        }
    }

    /// The column names, in order (`<Table>Info::Columns`).
    pub fn column_names(self) -> Vec<&'static str> {
        self.columns().iter().map(|(name, _)| *name).collect()
    }

    /// The column types, in order (`<Table>Info::Types`).
    pub fn types(self) -> Vec<LogicalType> {
        self.columns().iter().map(|(_, kind)| kind.logical_type()).collect()
    }

    /// The table's schema, as `CreateTPCHTable` declares it.
    pub fn schema(self) -> TpchTableSchema {
        TpchTableSchema {
            table: self,
            name: self.name(),
            columns: self
                .columns()
                .iter()
                .map(|&(name, kind)| TpchColumn {
                    name,
                    ty: kind.logical_type(),
                    not_null: true,
                })
                .collect(),
        }
    }
}

impl fmt::Display for TpchTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A column of a TPC-H table.
#[derive(Debug, Clone, PartialEq)]
pub struct TpchColumn {
    pub name: &'static str,
    pub ty: LogicalType,
    /// `CreateTPCHTable` adds a `NotNullConstraint` on every column.
    pub not_null: bool,
}

/// A TPC-H table as `CreateTPCHTable` creates it: `CREATE TABLE IF NOT EXISTS <name><suffix>
/// (<column> <type> NOT NULL, ...)` (the C++ uses `OnCreateConflict::IGNORE_ON_CONFLICT`, then
/// checks the table with [`validate_tpch_table_schema`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TpchTableSchema {
    pub table: TpchTable,
    pub name: &'static str,
    pub columns: Vec<TpchColumn>,
}

/// The schema `DBGenWrapper::CreateTPCHSchema` creates: the 8 tables, in its order.
pub fn tpch_schema() -> Vec<TpchTableSchema> {
    TpchTable::ALL.iter().map(|t| t.schema()).collect()
}

/// `ValidateTPCHTableSchema`: checks that an existing table `table_name` (the name with its
/// suffix) has the columns of `table`, given as its `(name, type)` pairs.
pub fn validate_tpch_table_schema(table: TpchTable, table_name: &str, columns: &[(&str, &LogicalType)]) -> Result<()> {
    let expected = table.columns();
    if columns.len() != expected.len() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "TPC-H table \"{table_name}\" has an incompatible schema: expected {} columns but found {}",
                expected.len(),
                columns.len()
            ),
        ));
    }
    for (i, (&(name, ty), &(expected_name, kind))) in columns.iter().zip(expected).enumerate() {
        if !name.eq_ignore_ascii_case(expected_name) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "TPC-H table \"{table_name}\" has an incompatible schema: expected column \"{expected_name}\" at position {i} but found {name}"
                ),
            ));
        }
        let expected_type = kind.logical_type();
        if *ty != expected_type {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "TPC-H table \"{table_name}\" has an incompatible schema: column \"{expected_name}\" has type {ty} but expected {expected_type}"
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Appending rows (`tpch_append_information`)
// ---------------------------------------------------------------------------------------------

/// The rows of one column of the chunk being built.
enum ColumnBuffer {
    Int32(Vec<i32>),
    /// BIGINT, and DECIMAL(15, 2) as its unscaled value
    Int64(Vec<i64>),
    Date(Vec<Date>),
    /// the bytes of the strings, and where each ends
    Varchar {
        bytes: Vec<u8>,
        ends: Vec<usize>,
    },
}

/// The chunk being built for one table (`tpch_append_information`).
struct TableAppendInfo {
    table: TpchTable,
    types: Vec<LogicalType>,
    columns: Vec<ColumnBuffer>,
    row: usize,
    active_col: usize,
}

impl TableAppendInfo {
    fn new(table: TpchTable) -> Self {
        let columns = table
            .columns()
            .iter()
            .map(|(_, kind)| match kind {
                ColumnKind::Integer => ColumnBuffer::Int32(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
                ColumnKind::BigInt | ColumnKind::Decimal => {
                    ColumnBuffer::Int64(Vec::with_capacity(STANDARD_VECTOR_SIZE))
                }
                ColumnKind::Date => ColumnBuffer::Date(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
                ColumnKind::Varchar => ColumnBuffer::Varchar {
                    bytes: Vec::new(),
                    ends: Vec::with_capacity(STANDARD_VECTOR_SIZE),
                },
            })
            .collect();
        Self {
            table,
            types: table.types(),
            columns,
            row: 0,
            active_col: 0,
        }
    }

    /// `FlushChunk`: the rows appended so far, as a chunk.
    fn flush_chunk(&mut self) -> Option<DataChunk> {
        if self.row == 0 {
            return None;
        }
        let mut chunk = DataChunk::new(&self.types);
        for (c, column) in self.columns.iter_mut().enumerate() {
            let vector = chunk.column_mut(c);
            match column {
                ColumnBuffer::Int32(values) => {
                    vector.data_mut::<i32>()[..values.len()].copy_from_slice(values);
                    values.clear();
                }
                ColumnBuffer::Int64(values) => {
                    vector.data_mut::<i64>()[..values.len()].copy_from_slice(values);
                    values.clear();
                }
                ColumnBuffer::Date(values) => {
                    vector.data_mut::<Date>()[..values.len()].copy_from_slice(values);
                    values.clear();
                }
                ColumnBuffer::Varchar { bytes, ends } => {
                    let capacity = bytes.capacity();
                    vector.set_strs_owned(std::mem::replace(bytes, Vec::with_capacity(capacity)), ends);
                    ends.clear();
                }
            }
        }
        chunk.set_len(self.row);
        self.row = 0;
        self.active_col = 0;
        Some(chunk)
    }

    fn next_column(&mut self) -> &mut ColumnBuffer {
        let column = &mut self.columns[self.active_col];
        self.active_col += 1;
        column
    }

    /// `append_end_row`
    fn end_row(&mut self) {
        debug_assert_eq!(self.active_col, self.columns.len());
        self.row += 1;
        self.active_col = 0;
    }

    /// `append_int32`
    fn append_int32(&mut self, value: i32) {
        match self.next_column() {
            ColumnBuffer::Int32(values) => values.push(value),
            _ => unreachable!("not an INTEGER column"),
        }
    }

    /// `append_int64`, `append_decimal`
    fn append_int64(&mut self, value: i64) {
        match self.next_column() {
            ColumnBuffer::Int64(values) => values.push(value),
            _ => unreachable!("not a BIGINT or DECIMAL column"),
        }
    }

    /// `append_date`: dbgen dates are raw day offsets from STARTDATE; dates are days since the Unix epoch.
    fn append_date(&mut self, value: DssHuge) {
        const UNIX_EPOCH_OFFSET: DssHuge = 8035;
        match self.next_column() {
            ColumnBuffer::Date(values) => values.push(Date((value - STARTDATE + UNIX_EPOCH_OFFSET) as i32)),
            _ => unreachable!("not a DATE column"),
        }
    }

    /// Appends a string that `write` writes (`append_empty_string` + `Finalize`).
    #[inline]
    fn append_string_with(&mut self, write: impl FnOnce(&mut Vec<u8>)) {
        match self.next_column() {
            ColumnBuffer::Varchar { bytes, ends } => {
                write(bytes);
                ends.push(bytes.len());
            }
            _ => unreachable!("not a VARCHAR column"),
        }
    }

    /// `append_string_reference`
    #[inline]
    fn append_string(&mut self, value: &[u8]) {
        self.append_string_with(|bytes| bytes.extend_from_slice(value));
    }

    /// `append_char`
    fn append_char(&mut self, value: u8) {
        self.append_string(&[value]);
    }
}

/// `PaddedNumberWidth`
fn padded_number_width(value: DssHuge, min_width: usize) -> usize {
    let mut width = 1;
    let mut remaining = value;
    while remaining >= 10 {
        width += 1;
        remaining /= 10;
    }
    width.max(min_width)
}

/// `WritePaddedNumber`: the last `width` decimal digits of `value`.
fn write_padded_number(target: &mut Vec<u8>, mut value: DssHuge, width: usize) {
    let start = target.len();
    target.resize(start + width, b'0');
    for i in (0..width).rev() {
        target[start + i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
}

/// `AppendTaggedNumber`
fn append_tagged_number(info: &mut TableAppendInfo, tag: &[u8], value: DssHuge, min_width: usize) {
    info.append_string_with(|bytes| {
        bytes.extend_from_slice(tag);
        write_padded_number(bytes, value, padded_number_width(value, min_width));
    });
}

/// `AppendPhone`
fn append_phone(info: &mut TableAppendInfo, nation_code: DssHuge, seed: &mut Seed) {
    let acode = random(100, 999, seed);
    let exchg = random(100, 999, seed);
    let number = random(1000, 9999, seed);
    info.append_string_with(|bytes| {
        let start = bytes.len();
        write_padded_number(bytes, 10 + (nation_code % NATIONS_MAX), 2);
        bytes.push(b'-');
        write_padded_number(bytes, acode, 3);
        bytes.push(b'-');
        write_padded_number(bytes, exchg, 3);
        bytes.push(b'-');
        write_padded_number(bytes, number, 4);
        debug_assert_eq!(bytes.len() - start, PHONE_LEN);
    });
}

/// `AppendRandomAlphaNumeric`
fn append_random_alpha_numeric(info: &mut TableAppendInfo, average_length: i32, seed: &mut Seed) {
    const ALPHA_NUM: &[u8; 64] = b"0123456789abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ,";
    let (low, high) = v_str_bounds(average_length);
    let length = random(DssHuge::from(low), DssHuge::from(high), seed);
    info.append_string_with(|bytes| {
        let mut char_int: DssHuge = 0;
        for i in 0..length {
            if i % 5 == 0 {
                char_int = random(0, MAX_LONG, seed);
            }
            bytes.push(ALPHA_NUM[(char_int & 0o77) as usize]);
            char_int >>= 6;
        }
    });
}

/// The distributions and the text pool a generator reads.
struct Env {
    dists: &'static Dists,
    pool: Arc<TextPool>,
}

/// `AppendText`
fn append_text(info: &mut TableAppendInfo, env: &Env, average_length: i32, seed: &mut Seed) {
    let (low, high) = v_str_bounds(average_length);
    info.append_string(env.pool.dbg_text_source(low, high, seed));
}

/// `AppendDistribution`
fn append_distribution(info: &mut TableAppendInfo, set: &Distribution, seed: &mut Seed) {
    info.append_string(pick_distribution(set, seed).as_bytes());
}

/// `RetailPrice`
fn retail_price(part_key: DssHuge) -> DssHuge {
    let mut price = 90000;
    price += (part_key / 10) % 20001;
    price += (part_key % 1000) * 100;
    price
}

/// `PartSuppBridge`
fn part_supp_bridge(ctx: &DBGenContext, part_key: DssHuge, supp_num: DssHuge) -> DssHuge {
    let total_supplier_count = ctx.tdefs[SUPP].base * ctx.scale_factor;
    (part_key + supp_num * (total_supplier_count / SUPP_PER_PART + (part_key - 1) / total_supplier_count))
        % total_supplier_count
        + 1
}

/// `AppendPartName`
fn append_part_name(info: &mut TableAppendInfo, env: &Env, ctx: &mut DBGenContext) {
    let colors = &env.dists.colors;
    permute_dist(colors, P_NAME_SCL, P_NAME_SD, ctx);
    info.append_string_with(|bytes| {
        for (i, &member) in ctx.permute[..P_NAME_SCL].iter().enumerate() {
            if i > 0 {
                bytes.push(b' ');
            }
            bytes.extend_from_slice(colors.member(member as usize).as_bytes());
        }
    });
}

/// `AppendSupplierComment`: supplier comments can be patched with BBB text.
fn append_supplier_comment(info: &mut TableAppendInfo, env: &Env, ctx: &mut DBGenContext) {
    let (low, high) = v_str_bounds(S_CMNT_LEN);
    let source = env.pool.dbg_text_source(low, high, &mut ctx.seed[S_CMNT_SD]);
    let length = source.len() as DssHuge;
    let bad_press = random(1, 10000, &mut ctx.seed[BBB_CMNT_SD]);
    let mut kind = random(0, 100, &mut ctx.seed[BBB_TYPE_SD]);
    let noise = random(0, length - BBB_CMNT_LEN, &mut ctx.seed[BBB_JNK_SD]);
    let offset = random(0, length - (BBB_CMNT_LEN + noise), &mut ctx.seed[BBB_OFFSET_SD]);
    info.append_string_with(|bytes| {
        let start = bytes.len();
        bytes.extend_from_slice(source);
        if bad_press <= S_CMNT_BBB {
            kind = if kind < BBB_DEADBEATS { 0 } else { 1 };
            let target = &mut bytes[start..];
            let offset = offset as usize;
            target[offset..offset + BBB_BASE_LEN].copy_from_slice(BBB_BASE);
            let at = BBB_BASE_LEN + offset + noise as usize;
            let text = if kind == 0 { BBB_COMPLAIN } else { BBB_COMMEND };
            target[at..at + BBB_TYPE_LEN].copy_from_slice(text);
        }
    });
}

/// The chunks being built for every table, and the chunks finished so far.
struct AppendInfos {
    tables: Vec<TableAppendInfo>,
    output: VecDeque<(TpchTable, DataChunk)>,
}

impl AppendInfos {
    fn new() -> Self {
        let mut tables: Vec<TableAppendInfo> = Vec::with_capacity(REGION + 1);
        // indexed by dbgen table number; ORDER_LINE and PART_PSUPP are not physical tables
        for index in PART..=REGION {
            let table = match index {
                ORDER_LINE => TpchTable::Orders,
                PART_PSUPP => TpchTable::Part,
                _ => TpchTable::ALL
                    .iter()
                    .copied()
                    .find(|t| t.index() == index)
                    .expect("a dbgen table"),
            };
            tables.push(TableAppendInfo::new(table));
        }
        Self {
            tables,
            output: VecDeque::new(),
        }
    }

    /// `append_begin_row`: the table to append a row to, flushing its chunk when it is full.
    #[inline]
    fn begin_row(&mut self, t: usize) -> &mut TableAppendInfo {
        let info = &mut self.tables[t];
        debug_assert_eq!(info.active_col, 0);
        if info.row >= STANDARD_VECTOR_SIZE
            && let Some(chunk) = info.flush_chunk()
        {
            self.output.push_back((info.table, chunk));
        }
        &mut self.tables[t]
    }

    /// `TPCHDataAppender::Flush`: flushes the incomplete chunks, in table number order.
    fn flush(&mut self) {
        for t in PART..=REGION {
            if t == ORDER_LINE || t == PART_PSUPP {
                continue;
            }
            let info = &mut self.tables[t];
            if let Some(chunk) = info.flush_chunk() {
                self.output.push_back((info.table, chunk));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The row generators
// ---------------------------------------------------------------------------------------------

/// `GenerateOrderLine`: order `index` and its line items.
fn generate_order_line(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let current_date_raw = STARTDATE + unjulian(CURRENTDATE);
    let dists = env.dists;
    let order_key = mk_sparse(index, 0);

    let o_ckey_max = ctx.tdefs[CUST].base * ctx.scale_factor;
    let mut cust_key = if ctx.scale_factor >= 30000 {
        dss_random64(O_CKEY_MIN, o_ckey_max, &mut ctx.seed[O_CKEY_SD])
    } else {
        random(O_CKEY_MIN, o_ckey_max, &mut ctx.seed[O_CKEY_SD])
    };
    let mut delta = 1;
    while cust_key % CUST_MORTALITY == 0 {
        cust_key += delta;
        cust_key = cust_key.min(o_ckey_max);
        delta *= -1;
    }

    let order_date = random(O_ODATE_MIN, O_ODATE_MAX, &mut ctx.seed[O_ODATE_SD]);
    let order_priority = pick_distribution(&dists.o_priority_set, &mut ctx.seed[O_PRIO_SD]);
    let clerk_number = random(
        1,
        (ctx.scale_factor * O_CLRK_SCL).max(O_CLRK_SCL),
        &mut ctx.seed[O_CLRK_SD],
    );
    let (low, high) = v_str_bounds(O_CMNT_LEN);
    let order_comment = env.pool.dbg_text_source(low, high, &mut ctx.seed[O_CMNT_SD]);

    let line_count = random(O_LCNT_MIN, O_LCNT_MAX, &mut ctx.seed[O_LCNT_SD]);
    let l_pkey_max = ctx.tdefs[PART].base * ctx.scale_factor;
    let (line_low, line_high) = v_str_bounds(L_CMNT_LEN);

    let mut total_price: DssHuge = 0;
    let mut closed_line_count = 0;
    for line_number in 0..line_count {
        let mut quantity = random(L_QTY_MIN, L_QTY_MAX, &mut ctx.seed[L_QTY_SD]);
        let discount = random(L_DCNT_MIN, L_DCNT_MAX, &mut ctx.seed[L_DCNT_SD]);
        let tax = random(L_TAX_MIN, L_TAX_MAX, &mut ctx.seed[L_TAX_SD]);
        let ship_instruct = pick_distribution(&dists.l_instruct_set, &mut ctx.seed[L_SHIP_SD]);
        let ship_mode = pick_distribution(&dists.l_smode_set, &mut ctx.seed[L_SMODE_SD]);
        let line_comment = env.pool.dbg_text_source(line_low, line_high, &mut ctx.seed[L_CMNT_SD]);
        let part_key = if ctx.scale_factor >= 30000 {
            dss_random64(L_PKEY_MIN, l_pkey_max, &mut ctx.seed[L_PKEY_SD])
        } else {
            random(L_PKEY_MIN, l_pkey_max, &mut ctx.seed[L_PKEY_SD])
        };
        let retail_price = retail_price(part_key);
        let supp_num = random(0, 3, &mut ctx.seed[L_SKEY_SD]);
        let supp_key = part_supp_bridge(ctx, part_key, supp_num);
        quantity *= 100;
        let extended_price = retail_price * quantity / 100;
        total_price += ((extended_price * (100 - discount)) / PENNIES) * (100 + tax) / PENNIES;

        let ship_date = random(L_SDTE_MIN, L_SDTE_MAX, &mut ctx.seed[L_SDTE_SD]) + order_date;
        let commit_date = random(L_CDTE_MIN, L_CDTE_MAX, &mut ctx.seed[L_CDTE_SD]) + order_date;
        let receipt_date = random(L_RDTE_MIN, L_RDTE_MAX, &mut ctx.seed[L_RDTE_SD]) + ship_date;

        let mut return_flag = b'N';
        if receipt_date <= current_date_raw {
            return_flag = pick_distribution(&dists.l_rflag_set, &mut ctx.seed[L_RFLG_SD]).as_bytes()[0];
        }
        let line_status = if ship_date <= current_date_raw {
            closed_line_count += 1;
            b'F'
        } else {
            b'O'
        };

        let line_info = info.begin_row(LINE);
        line_info.append_int64(order_key);
        line_info.append_int64(part_key);
        line_info.append_int64(supp_key);
        line_info.append_int64(line_number + 1);
        line_info.append_int64(quantity);
        line_info.append_int64(extended_price);
        line_info.append_int64(discount);
        line_info.append_int64(tax);
        line_info.append_char(return_flag);
        line_info.append_char(line_status);
        line_info.append_date(ship_date);
        line_info.append_date(commit_date);
        line_info.append_date(receipt_date);
        line_info.append_string(ship_instruct.as_bytes());
        line_info.append_string(ship_mode.as_bytes());
        line_info.append_string(line_comment);
        line_info.end_row();
    }

    let mut order_status = b'O';
    if closed_line_count > 0 {
        order_status = b'P';
    }
    if closed_line_count == line_count {
        order_status = b'F';
    }

    let order_info = info.begin_row(ORDER);
    order_info.append_int64(order_key);
    order_info.append_int64(cust_key);
    order_info.append_char(order_status);
    order_info.append_int64(total_price);
    order_info.append_date(order_date);
    order_info.append_string(order_priority.as_bytes());
    append_tagged_number(order_info, O_CLRK_TAG, clerk_number, 9);
    order_info.append_int32(0);
    order_info.append_string(order_comment);
    order_info.end_row();
}

/// `GenerateSupplier`
fn generate_supplier(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let append_info = info.begin_row(SUPP);
    append_info.append_int64(index);
    append_tagged_number(append_info, S_NAME_TAG, index, 9);
    append_random_alpha_numeric(append_info, S_ADDR_LEN, &mut ctx.seed[S_ADDR_SD]);
    let nation_code = random(0, env.dists.nations.count as DssHuge - 1, &mut ctx.seed[S_NTRG_SD]);
    append_info.append_int32(nation_code as i32);
    append_phone(append_info, nation_code, &mut ctx.seed[S_PHNE_SD]);
    let acctbal = random(S_ABAL_MIN, S_ABAL_MAX, &mut ctx.seed[S_ABAL_SD]);
    append_info.append_int64(acctbal);
    append_supplier_comment(append_info, env, ctx);
    append_info.end_row();
}

/// `GenerateCustomer`
fn generate_customer(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let append_info = info.begin_row(CUST);
    append_info.append_int64(index);
    append_tagged_number(append_info, C_NAME_TAG, index, 9);
    append_random_alpha_numeric(append_info, C_ADDR_LEN, &mut ctx.seed[C_ADDR_SD]);
    let nation_code = random(0, env.dists.nations.count as DssHuge - 1, &mut ctx.seed[C_NTRG_SD]);
    append_info.append_int32(nation_code as i32);
    append_phone(append_info, nation_code, &mut ctx.seed[C_PHNE_SD]);
    let acctbal = random(C_ABAL_MIN, C_ABAL_MAX, &mut ctx.seed[C_ABAL_SD]);
    append_info.append_int64(acctbal);
    append_distribution(append_info, &env.dists.c_mseg_set, &mut ctx.seed[C_MSEG_SD]);
    append_text(append_info, env, C_CMNT_LEN, &mut ctx.seed[C_CMNT_SD]);
    append_info.end_row();
}

/// `GeneratePartAndPartsupp`
fn generate_part_and_partsupp(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let dists = env.dists;
    let part_info = info.begin_row(PART);
    part_info.append_int64(index);
    append_part_name(part_info, env, ctx);
    let manufacturer = random(P_MFG_MIN, P_MFG_MAX, &mut ctx.seed[P_MFG_SD]);
    // AppendManufacturer
    append_tagged_number(part_info, P_MFG_TAG, manufacturer, 1);
    let brand = random(P_BRND_MIN, P_BRND_MAX, &mut ctx.seed[P_BRND_SD]);
    // AppendBrand
    append_tagged_number(part_info, P_BRND_TAG, manufacturer * 10 + brand, 2);
    append_distribution(part_info, &dists.p_types_set, &mut ctx.seed[P_TYPE_SD]);
    let size = random(P_SIZE_MIN, P_SIZE_MAX, &mut ctx.seed[P_SIZE_SD]);
    part_info.append_int32(size as i32);
    append_distribution(part_info, &dists.p_cntr_set, &mut ctx.seed[P_CNTR_SD]);
    part_info.append_int64(retail_price(index));
    append_text(part_info, env, P_CMNT_LEN, &mut ctx.seed[P_CMNT_SD]);
    part_info.end_row();

    for supp_num in 0..SUPP_PER_PART {
        let supp_key = part_supp_bridge(ctx, index, supp_num);
        let partsupp_info = info.begin_row(PSUPP);
        partsupp_info.append_int64(index);
        partsupp_info.append_int64(supp_key);
        let quantity = random(PS_QTY_MIN, PS_QTY_MAX, &mut ctx.seed[PS_QTY_SD]);
        partsupp_info.append_int64(quantity);
        let supply_cost = random(PS_SCST_MIN, PS_SCST_MAX, &mut ctx.seed[PS_SCST_SD]);
        partsupp_info.append_int64(supply_cost);
        append_text(partsupp_info, env, PS_CMNT_LEN, &mut ctx.seed[PS_CMNT_SD]);
        partsupp_info.end_row();
    }
}

/// `GenerateNation`
fn generate_nation(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let nation = env.dists.nations.list[index as usize - 1];
    let append_info = info.begin_row(NATION);
    append_info.append_int32((index - 1) as i32);
    append_info.append_string(nation.text.as_bytes());
    append_info.append_int32(nation.weight as i32);
    append_text(append_info, env, N_CMNT_LEN, &mut ctx.seed[N_CMNT_SD]);
    append_info.end_row();
}

/// `GenerateRegion`
fn generate_region(index: DssHuge, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    let region = env.dists.regions.list[index as usize - 1];
    let append_info = info.begin_row(REGION);
    append_info.append_int32((index - 1) as i32);
    append_info.append_string(region.text.as_bytes());
    append_text(append_info, env, R_CMNT_LEN, &mut ctx.seed[R_CMNT_SD]);
    append_info.end_row();
}

/// `gen_tbl`: rows `offset + 1 ..= offset + count` of table `tnum`.
fn gen_tbl(tnum: usize, count: usize, offset: usize, info: &mut AppendInfos, env: &Env, ctx: &mut DBGenContext) {
    for i in offset + 1..=offset + count {
        let i = i as DssHuge;
        row_start(tnum, ctx);
        match tnum {
            LINE | ORDER | ORDER_LINE => generate_order_line(i, info, env, ctx),
            SUPP => generate_supplier(i, info, env, ctx),
            CUST => generate_customer(i, info, env, ctx),
            PSUPP | PART | PART_PSUPP => generate_part_and_partsupp(i, info, env, ctx),
            NATION => generate_nation(i, info, env, ctx),
            REGION => generate_region(i, info, env, ctx),
            _ => unreachable!("unknown dbgen table {tnum}"),
        }
        row_stop_h(tnum, ctx);
    }
}

/// `skip`: advances the seeds of `table` past `step` rows.
fn skip(table: usize, children: i32, step: DssHuge, ctx: &mut DBGenContext) {
    match table {
        CUST => sd_cust(children, step, ctx),
        SUPP => sd_supp(children, step, ctx),
        NATION => sd_nation(children, step, ctx),
        REGION => sd_region(children, step, ctx),
        ORDER_LINE => {
            sd_line(children, step, ctx);
            sd_order(children, step, ctx);
        }
        PART_PSUPP => {
            sd_part(children, step, ctx);
            sd_psupp(children, step, ctx);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------------------------

const DBGEN_TABLES: [usize; 6] = [SUPP, CUST, ORDER_LINE, PART_PSUPP, NATION, REGION];
/// The rows [`DBGenGenerator::next_chunk`] generates before it looks for a finished chunk again.
const NEXT_CHUNK_BATCH_SIZE: usize = 64;

/// `DBGenTableRange`
#[derive(Debug, Clone, Copy)]
struct DBGenTableRange {
    offset: usize,
    count: usize,
}

/// `GetDBGenTableRowCount`
fn get_dbgen_table_row_count(dbgen_ctx: &DBGenContext, table_index: usize) -> usize {
    if table_index < NATION {
        return (dbgen_ctx.tdefs[table_index].base * dbgen_ctx.scale_factor) as usize;
    }
    dbgen_ctx.tdefs[table_index].base as usize
}

/// `GetDBGenTableRange`
fn get_dbgen_table_range(row_count: usize, children: i32, current_step: i32) -> DBGenTableRange {
    if children <= 1 || current_step == -1 {
        return DBGenTableRange {
            offset: 0,
            count: row_count,
        };
    }
    let child_count = children as usize;
    let step = current_step as usize;
    let part_size = row_count.div_ceil(child_count);
    let part_offset = part_size * step;
    if part_offset >= row_count {
        return DBGenTableRange {
            offset: part_offset,
            count: 0,
        };
    }
    DBGenTableRange {
        offset: part_offset,
        count: part_size.min(row_count - part_offset),
    }
}

/// `GetDBGenTotalWork`
fn get_dbgen_total_work(dbgen_ctx: &DBGenContext, children: i32, start_step: i32, end_step: i32) -> usize {
    let mut total = 0;
    for step in start_step..end_step {
        for &table_index in &DBGEN_TABLES {
            let row_count = get_dbgen_table_row_count(dbgen_ctx, table_index);
            total += get_dbgen_table_range(row_count, children, step).count;
        }
    }
    total
}

/// `TPCHDataAppender`: one step's generation, with its own copy of the seeds.
struct TPCHDataAppender {
    dbgen_ctx: DBGenContext,
    append_info: AppendInfos,
    generated_work: usize,
}

impl TPCHDataAppender {
    fn new(base_context: &DBGenContext) -> Self {
        Self {
            dbgen_ctx: base_context.clone(),
            append_info: AppendInfos::new(),
            generated_work: 0,
        }
    }

    /// `GenerateTableData`
    fn generate_table_data(&mut self, env: &Env, table_index: usize, row_count: usize, offset: usize) {
        if row_count == 0 {
            return;
        }
        gen_tbl(
            table_index,
            row_count,
            offset,
            &mut self.append_info,
            env,
            &mut self.dbgen_ctx,
        );
        self.generated_work += row_count;
    }

    /// `SkipTable`
    fn skip_table(&mut self, table_index: usize, children: i32, offset: usize) {
        if children > 1 {
            skip(table_index, children, offset as DssHuge, &mut self.dbgen_ctx);
        }
    }
}

/// The TPC-H data generator (`TPCHDBGenGenerator`, made by `tpch::CreateDBGenGenerator`).
///
/// It yields the rows of every table as [`DataChunk`]s of at most [`STANDARD_VECTOR_SIZE`] rows,
/// each tagged with its table, with the types of [`TpchTable::types`].
pub struct DBGenGenerator {
    flt_scale: f64,
    children: i32,
    current_step: i32,
    base_context: DBGenContext,
    env: Option<Env>,
    finished: bool,
    generated_work: usize,
    flushed_work: usize,
    total_work: usize,

    sequential_children: i32,
    sequential_end_step: i32,
    sequential_step: i32,
    sequential_table: usize,
    sequential_table_started: bool,
    sequential_table_offset: usize,
    sequential_table_count: usize,
    sequential_table_generated: usize,
    sequential_appender: Option<TPCHDataAppender>,
    output: VecDeque<(TpchTable, DataChunk)>,
}

impl DBGenGenerator {
    /// A generator for scale factor `sf`. With `children` > 1, generates only part `step` (0-based)
    /// of the `children` parts of every table, as `CALL dbgen(sf = ..., children = ..., step = ...)`.
    ///
    /// Errors as `DbgenBind` and the `TPCHDBGenGenerator` constructor do: a `children` other than 1
    /// without a `step`, a NaN scale factor, or one above 100000. A scale factor of 0 or less, or a
    /// `step` of `children` or more, generates nothing (the tables stay empty).
    pub fn new(sf: f64, children: u32, step: Option<u32>) -> Result<Self> {
        // DbgenBind
        if children != 1 && step.is_none() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Step must be defined when children are defined",
            ));
        }
        // the C++ passes both to the generator as int
        let children = children as i32;
        let current_step = step.map_or(-1, |s| s as i32);
        let mut generator = Self {
            flt_scale: sf,
            children,
            current_step,
            base_context: DBGenContext::new(),
            env: None,
            finished: false,
            generated_work: 0,
            flushed_work: 0,
            total_work: 0,
            sequential_children: 1,
            sequential_end_step: 0,
            sequential_step: 0,
            sequential_table: 0,
            sequential_table_started: false,
            sequential_table_offset: 0,
            sequential_table_count: 0,
            sequential_table_generated: 0,
            sequential_appender: None,
            output: VecDeque::new(),
        };
        // NaN compares false against every scale factor check, and reaches an out-of-range cast below
        if sf.is_nan() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "DBGen requires a valid scale factor.",
            ));
        }
        if sf <= 0.0 {
            generator.finish();
            return Ok(generator);
        }
        if sf > MAX_SCALE {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("DBGen does not support a scale factor exceeding {}.", MAX_SCALE as i32),
            ));
        }

        generator.initialize_base_context();

        if current_step >= children {
            generator.finish();
            return Ok(generator);
        }

        let dists = load_dists();
        let pool = TextPool::get(dists, &generator.base_context);
        generator.env = Some(Env { dists, pool });
        // have to do this after init
        generator.base_context.tdefs[NATION].base = dists.nations.count as DssHuge;
        generator.base_context.tdefs[REGION].base = dists.regions.count as DssHuge;

        let explicit = generator.explicit_partial_generation();
        generator.sequential_children = if explicit {
            children
        } else {
            generator.get_default_child_count() as i32
        };
        let sequential_start_step = if explicit { current_step } else { 0 };
        generator.sequential_end_step = if explicit {
            current_step + 1
        } else {
            generator.sequential_children
        };
        generator.sequential_step = sequential_start_step;
        generator.total_work = get_dbgen_total_work(
            &generator.base_context,
            generator.sequential_children,
            sequential_start_step,
            generator.sequential_end_step,
        );
        Ok(generator)
    }

    /// The next chunk of generated rows and its table, or `None` once every table is generated.
    pub fn next_chunk(&mut self) -> Option<(TpchTable, DataChunk)> {
        loop {
            if let Some(chunk) = self.output.pop_front() {
                return Some(chunk);
            }
            if let Some(chunk) = self
                .sequential_appender
                .as_mut()
                .and_then(|a| a.append_info.output.pop_front())
            {
                return Some(chunk);
            }
            if self.finished {
                return None;
            }
            self.generate_next(NEXT_CHUNK_BATCH_SIZE);
        }
    }

    /// Whether every row has been generated and handed out.
    pub fn is_finished(&self) -> bool {
        self.finished
            && self.output.is_empty()
            && self
                .sequential_appender
                .as_ref()
                .is_none_or(|a| a.append_info.output.is_empty())
    }

    /// The progress percentage in `[0, 100]` (`Progress`).
    pub fn progress(&self) -> f64 {
        if self.finished {
            return 100.0;
        }
        if self.total_work == 0 {
            return 0.0;
        }
        let current_generated_work = self.generated_work.min(self.total_work);
        let current_flushed_work = self.flushed_work.min(self.total_work);
        let generated_progress = current_generated_work as f64 / self.total_work as f64;
        let flushed_progress = current_flushed_work as f64 / self.total_work as f64;
        // the C++ weighs flushing 0.05 in its parallel mode
        let flush_weight = 0.5;
        let current_progress = 100.0 * (generated_progress * (1.0 - flush_weight) + flushed_progress * flush_weight);
        current_progress.min(99.9)
    }

    /// `GenerateNext`: generates up to `batch_size` more rows of the current table.
    fn generate_next(&mut self, batch_size: usize) -> bool {
        if self.finished {
            return true;
        }
        if self.total_work == 0 {
            self.finish();
            return true;
        }
        self.generate_sequential(batch_size)
    }

    /// `InitializeBaseContext`
    fn initialize_base_context(&mut self) {
        let tdefs = &mut self.base_context.tdefs;
        tdefs[PART].base = 200000;
        tdefs[PSUPP].base = 200000;
        tdefs[SUPP].base = 10000;
        tdefs[CUST].base = 150000;
        tdefs[ORDER].base = 150000 * ORDERS_PER_CUST;
        tdefs[LINE].base = 150000 * ORDERS_PER_CUST;
        tdefs[ORDER_LINE].base = 150000 * ORDERS_PER_CUST;
        tdefs[PART_PSUPP].base = 200000;
        tdefs[NATION].base = NATIONS_MAX;
        tdefs[REGION].base = NATIONS_MAX;

        if self.flt_scale < MIN_SCALE {
            let int_scale = (1000.0 * self.flt_scale) as i32;
            self.base_context.scale_factor = 1;
            for tdef in &mut tdefs[PART..REGION] {
                tdef.base = (DssHuge::from(int_scale) * tdef.base) / 1000;
                if tdef.base < 1 {
                    tdef.base = 1;
                }
            }
        } else {
            self.base_context.scale_factor = self.flt_scale as i64;
        }
    }

    /// `ExplicitPartialGeneration`
    fn explicit_partial_generation(&self) -> bool {
        self.children > 1 && self.current_step != -1
    }

    /// `GetDefaultChildCount`
    fn get_default_child_count(&self) -> usize {
        if self.flt_scale < 1.0 {
            return 1;
        }
        const CHILDREN_PER_SCALE_FACTOR: f64 = 20.0;
        ((CHILDREN_PER_SCALE_FACTOR * self.flt_scale) as usize).min(MAX_CHILDREN)
    }

    /// `InitializeSequentialAppender`
    fn initialize_sequential_appender(&mut self) {
        if self.sequential_appender.is_some() {
            return;
        }
        self.sequential_appender = Some(TPCHDataAppender::new(&self.base_context));
        self.sequential_table = 0;
        self.sequential_table_started = false;
    }

    /// Flushes the step's appender and moves its chunks to the output.
    fn flush_sequential_appender(&mut self) {
        if let Some(mut appender) = self.sequential_appender.take() {
            self.flushed_work += appender.generated_work;
            appender.append_info.flush();
            self.output.extend(appender.append_info.output.drain(..));
        }
    }

    /// `GenerateSequential`
    fn generate_sequential(&mut self, batch_size: usize) -> bool {
        let env = self.env.take().expect("a generator with work has its distributions");
        let finished = self.generate_sequential_with(&env, batch_size);
        if !finished {
            self.env = Some(env);
        }
        finished
    }

    fn generate_sequential_with(&mut self, env: &Env, batch_size: usize) -> bool {
        while self.sequential_step < self.sequential_end_step {
            self.initialize_sequential_appender();
            while self.sequential_table < DBGEN_TABLES.len() {
                let table_index = DBGEN_TABLES[self.sequential_table];
                let appender = self.sequential_appender.as_mut().expect("initialized above");
                if !self.sequential_table_started {
                    let row_count = get_dbgen_table_row_count(&self.base_context, table_index);
                    let range = get_dbgen_table_range(row_count, self.sequential_children, self.sequential_step);
                    self.sequential_table_offset = range.offset;
                    self.sequential_table_count = range.count;
                    self.sequential_table_generated = 0;
                    self.sequential_table_started = true;
                    appender.skip_table(table_index, self.sequential_children, self.sequential_table_offset);
                }
                if self.sequential_table_generated >= self.sequential_table_count {
                    self.sequential_table += 1;
                    self.sequential_table_started = false;
                    continue;
                }
                let remaining = self.sequential_table_count - self.sequential_table_generated;
                let generate_count = remaining.min(batch_size);
                appender.generate_table_data(
                    env,
                    table_index,
                    generate_count,
                    self.sequential_table_offset + self.sequential_table_generated,
                );
                self.sequential_table_generated += generate_count;
                self.generated_work += generate_count;
                if self.generated_work >= self.total_work {
                    continue;
                }
                return false;
            }
            self.flush_sequential_appender();
            self.sequential_step += 1;
        }
        self.finish();
        true
    }

    /// `Finish`
    fn finish(&mut self) {
        self.flush_sequential_appender();
        // cleanup_dists
        self.env = None;
        self.generated_work = self.total_work;
        self.flushed_work = self.total_work;
        self.finished = true;
    }
}

impl Iterator for DBGenGenerator {
    type Item = (TpchTable, DataChunk);

    fn next(&mut self) -> Option<Self::Item> {
        self.next_chunk()
    }
}

/// `tpch::CreateDBGenGenerator`
pub fn create_dbgen_generator(sf: f64, children: u32, step: Option<u32>) -> Result<DBGenGenerator> {
    DBGenGenerator::new(sf, children, step)
}

// ---------------------------------------------------------------------------------------------
// Queries and answers
// ---------------------------------------------------------------------------------------------

fn check_query_number(query: i32) -> Result<usize> {
    if query <= 0 || query > TPCH_QUERIES_COUNT {
        return Err(Error::new(
            ErrorKind::Syntax,
            format!("Out of range TPC-H query number {query}"),
        ));
    }
    Ok(query as usize - 1)
}

/// `DBGenWrapper::GetQuery(query)`: TPC-H query `query` (1 to 22), with the substitution parameters of SF 1.
pub fn get_query(query: i32) -> Result<&'static str> {
    Ok(TPCH_QUERIES[check_query_number(query)?])
}

/// The stored Q11 carries the FRACTION substitution parameter for SF 1
const TPCH_Q11_SF1_FRACTION: &str = "0.0001000000";

/// `DBGenWrapper::GetQuery(query, sf)`: TPC-H query `query` with the substitution parameters of
/// scale factor `sf`. Only Q11 differs between scale factors: it selects the parts whose stock value
/// exceeds FRACTION of the total, where qgen sets FRACTION = 0.0001 / SF.
pub fn get_query_for_sf(query: i32, sf: f64) -> Result<String> {
    if sf.is_nan() || sf <= 0.0 || sf > MAX_SCALE {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "TPC-H queries require a scale factor between 0 and {}",
                MAX_SCALE as i32
            ),
        ));
    }
    let result = get_query(query)?;
    if query != 11 {
        return Ok(result.to_string());
    }
    let fraction = format!("{:.10}", 0.0001 / sf);
    debug_assert!(result.contains(TPCH_Q11_SF1_FRACTION));
    Ok(result.replacen(TPCH_Q11_SF1_FRACTION, &fraction, 1))
}

/// The scale factors [`get_answer`] has answers for, in the order `tpch_answers()` returns them.
pub const TPCH_ANSWER_SCALE_FACTORS: [f64; 3] = [0.01, 0.1, 1.0];

/// `DBGenWrapper::GetAnswer`: the answer of query `query` at scale factor `sf` (0.01, 0.1 or 1), as
/// pipe-separated CSV with a header line.
pub fn get_answer(sf: f64, query: i32) -> Result<&'static str> {
    let index = check_query_number(query)?;
    let answers = if sf == 0.01 {
        &TPCH_ANSWERS_SF0_01
    } else if sf == 0.1 {
        &TPCH_ANSWERS_SF0_1
    } else if sf == 1.0 {
        &TPCH_ANSWERS_SF1
    } else {
        return Err(Error::not_implemented(format!(
            "Don't have TPC-H answers for SF {sf:.6}!"
        )));
    };
    Ok(answers[index])
}
