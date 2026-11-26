// Ported from duckdb/extension/tpch/dbgen/include/dbgen/dss.h, shared.h and config.h @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// The definitions and control information of the DSS generator. Only what the DuckDB generator
// path uses is kept; the flags the C++ keeps in globals (`table`, `set_seeds`, `verbose`, ...) are
// constants here, set the way `TPCHDBGenGenerator::InitializeBaseContext` sets them.

/// `DSS_HUGE`
pub type DssHuge = i64;

// table indexes
pub const NONE: i64 = -1;
pub const PART: usize = 0;
pub const PSUPP: usize = 1;
pub const SUPP: usize = 2;
pub const CUST: usize = 3;
pub const ORDER: usize = 4;
pub const LINE: usize = 5;
pub const ORDER_LINE: usize = 6;
pub const PART_PSUPP: usize = 7;
pub const NATION: usize = 8;
pub const REGION: usize = 9;

pub const MAX_CHILDREN: usize = 1000;
pub const MAX_LONG: DssHuge = 0x7FFF_FFFF;
pub const SPARSE_BITS: u32 = 2;
pub const SPARSE_KEEP: u32 = 3;

/// `RNG_A`, `RNG_C` of config.h
pub const RNG_A: u64 = 6364136223846793005;
pub const RNG_C: u64 = 1;

/// `set_seeds`: always 0 in DuckDB's dbgen
pub const SET_SEEDS: bool = false;

// text.c
pub const TEXT_POOL_SIZE: usize = 300 * 1024 * 1024;

// parts table
pub const P_NAME_SCL: usize = 5;
pub const P_MFG_TAG: &[u8] = b"Manufacturer#";
pub const P_MFG_MIN: DssHuge = 1;
pub const P_MFG_MAX: DssHuge = 5;
pub const P_BRND_TAG: &[u8] = b"Brand#";
pub const P_BRND_MIN: DssHuge = 1;
pub const P_BRND_MAX: DssHuge = 5;
pub const P_SIZE_MIN: DssHuge = 1;
pub const P_SIZE_MAX: DssHuge = 50;

// suppliers table
pub const S_NAME_TAG: &[u8] = b"Supplier#";
pub const S_ABAL_MIN: DssHuge = -99999;
pub const S_ABAL_MAX: DssHuge = 999999;
pub const S_CMNT_BBB: DssHuge = 10;
pub const BBB_DEADBEATS: DssHuge = 50;
pub const BBB_BASE: &[u8] = b"Customer ";
pub const BBB_COMPLAIN: &[u8] = b"Complaints";
pub const BBB_COMMEND: &[u8] = b"Recommends";
pub const BBB_CMNT_LEN: DssHuge = 19;
pub const BBB_BASE_LEN: usize = 9;
pub const BBB_TYPE_LEN: usize = 10;

// partsupp table
pub const PS_SCST_MIN: DssHuge = 100;
pub const PS_SCST_MAX: DssHuge = 100000;
pub const PS_QTY_MIN: DssHuge = 1;
pub const PS_QTY_MAX: DssHuge = 9999;

// customers table
pub const C_NAME_TAG: &[u8] = b"Customer#";
pub const C_ABAL_MIN: DssHuge = -99999;
pub const C_ABAL_MAX: DssHuge = 999999;

// order table
pub const O_CKEY_MIN: DssHuge = 1;
pub const O_ODATE_MIN: DssHuge = STARTDATE;
pub const O_ODATE_MAX: DssHuge = STARTDATE + TOTDATE - (L_SDTE_MAX + L_RDTE_MAX) - 1;
pub const O_CLRK_TAG: &[u8] = b"Clerk#";
pub const O_CLRK_SCL: DssHuge = 1000;
pub const O_LCNT_MIN: DssHuge = 1;
pub const O_LCNT_MAX: DssHuge = 7;

// lineitem table
pub const L_QTY_MIN: DssHuge = 1;
pub const L_QTY_MAX: DssHuge = 50;
pub const L_TAX_MIN: DssHuge = 0;
pub const L_TAX_MAX: DssHuge = 8;
pub const L_DCNT_MIN: DssHuge = 0;
pub const L_DCNT_MAX: DssHuge = 10;
pub const L_PKEY_MIN: DssHuge = 1;
pub const L_SDTE_MIN: DssHuge = 1;
pub const L_SDTE_MAX: DssHuge = 121;
pub const L_CDTE_MIN: DssHuge = 30;
pub const L_CDTE_MAX: DssHuge = 90;
pub const L_RDTE_MIN: DssHuge = 1;
pub const L_RDTE_MAX: DssHuge = 30;

/// `LEAP(y)`
pub const fn leap(y: i64) -> i64 {
    if y % 4 == 0 && y % 100 != 0 { 1 } else { 0 }
}

// general or inter table defines
pub const SUPP_PER_PART: DssHuge = 4;
pub const ORDERS_PER_CUST: DssHuge = 10;
pub const CUST_MORTALITY: DssHuge = 3;
pub const NATIONS_MAX: DssHuge = 90;
pub const STARTDATE: DssHuge = 92001;
pub const CURRENTDATE: DssHuge = 95168;
pub const TOTDATE: DssHuge = 2557;
pub const MAX_STREAM: usize = 47;
pub const V_STR_LOW: f64 = 0.4;
pub const PENNIES: DssHuge = 100;

pub const MIN_SCALE: f64 = 1.0;
pub const MAX_SCALE: f64 = 100000.0;

// shared.h
pub const N_CMNT_LEN: i32 = 72;
pub const R_CMNT_LEN: i32 = 72;
pub const V_STR_HGH: f64 = 1.6;
pub const P_CMNT_LEN: i32 = 14;
pub const S_ADDR_LEN: i32 = 25;
pub const S_CMNT_LEN: i32 = 63;
pub const PS_CMNT_LEN: i32 = 124;
pub const C_ADDR_LEN: i32 = 25;
pub const C_CMNT_LEN: i32 = 73;
pub const O_CMNT_LEN: i32 = 49;
pub const L_CMNT_LEN: i32 = 27;
pub const PHONE_LEN: usize = 15;

// seed indexes; used to separate the generation of individual columns
pub const P_MFG_SD: usize = 0;
pub const P_BRND_SD: usize = 1;
pub const P_TYPE_SD: usize = 2;
pub const P_SIZE_SD: usize = 3;
pub const P_CNTR_SD: usize = 4;
/// the seed of the text pool (`text pregeneration`)
pub const TEXT_SD: usize = 5;
pub const P_CMNT_SD: usize = 6;
pub const PS_QTY_SD: usize = 7;
pub const PS_SCST_SD: usize = 8;
pub const PS_CMNT_SD: usize = 9;
pub const O_SUPP_SD: usize = 10;
pub const O_CLRK_SD: usize = 11;
pub const O_CMNT_SD: usize = 12;
pub const O_ODATE_SD: usize = 13;
pub const L_QTY_SD: usize = 14;
pub const L_DCNT_SD: usize = 15;
pub const L_TAX_SD: usize = 16;
pub const L_SHIP_SD: usize = 17;
pub const L_SMODE_SD: usize = 18;
pub const L_PKEY_SD: usize = 19;
pub const L_SKEY_SD: usize = 20;
pub const L_SDTE_SD: usize = 21;
pub const L_CDTE_SD: usize = 22;
pub const L_RDTE_SD: usize = 23;
pub const L_RFLG_SD: usize = 24;
pub const L_CMNT_SD: usize = 25;
pub const C_ADDR_SD: usize = 26;
pub const C_NTRG_SD: usize = 27;
pub const C_PHNE_SD: usize = 28;
pub const C_ABAL_SD: usize = 29;
pub const C_MSEG_SD: usize = 30;
pub const C_CMNT_SD: usize = 31;
pub const S_ADDR_SD: usize = 32;
pub const S_NTRG_SD: usize = 33;
pub const S_PHNE_SD: usize = 34;
pub const S_ABAL_SD: usize = 35;
pub const S_CMNT_SD: usize = 36;
pub const P_NAME_SD: usize = 37;
pub const O_PRIO_SD: usize = 38;
pub const O_CKEY_SD: usize = 40;
pub const N_CMNT_SD: usize = 41;
pub const R_CMNT_SD: usize = 42;
pub const O_LCNT_SD: usize = 43;
pub const BBB_JNK_SD: usize = 44;
pub const BBB_TYPE_SD: usize = 45;
pub const BBB_CMNT_SD: usize = 46;
pub const BBB_OFFSET_SD: usize = 47;

/// One random number stream (`seed_t`).
#[derive(Debug, Clone, Copy)]
pub struct Seed {
    pub table: i64,
    pub value: DssHuge,
    pub usage: DssHuge,
    pub boundary: DssHuge,
}

/// A table definition (`tdef`); only the fields the generator reads.
#[derive(Debug, Clone, Copy)]
pub struct Tdef {
    pub base: DssHuge,
    pub child: i64,
}

const fn seed(table: i64, value: DssHuge, boundary: DssHuge) -> Seed {
    Seed {
        table,
        value,
        usage: 0,
        boundary,
    }
}

const P: i64 = PART as i64;
const PS: i64 = PSUPP as i64;
const O: i64 = ORDER as i64;
const L: i64 = LINE as i64;
const C: i64 = CUST as i64;
const S: i64 = SUPP as i64;

/// The initial seeds (`DBGenContext::Seed`).
pub const INITIAL_SEEDS: [Seed; MAX_STREAM + 1] = [
    seed(P, 1, 1),                           // P_MFG_SD     0
    seed(P, 46831694, 1),                    // P_BRND_SD    1
    seed(P, 1841581359, 1),                  // P_TYPE_SD    2
    seed(P, 1193163244, 1),                  // P_SIZE_SD    3
    seed(P, 727633698, 1),                   // P_CNTR_SD    4
    seed(NONE, 933588178, 1),                // text pregeneration  5
    seed(P, 804159733, 2),                   // P_CMNT_SD    6
    seed(PS, 1671059989, SUPP_PER_PART),     // PS_QTY_SD    7
    seed(PS, 1051288424, SUPP_PER_PART),     // PS_SCST_SD   8
    seed(PS, 1961692154, SUPP_PER_PART * 2), // PS_CMNT_SD   9
    seed(O, 1227283347, 1),                  // O_SUPP_SD    10
    seed(O, 1171034773, 1),                  // O_CLRK_SD    11
    seed(O, 276090261, 2),                   // O_CMNT_SD    12
    seed(O, 1066728069, 1),                  // O_ODATE_SD   13
    seed(L, 209208115, O_LCNT_MAX),          // L_QTY_SD     14
    seed(L, 554590007, O_LCNT_MAX),          // L_DCNT_SD    15
    seed(L, 721958466, O_LCNT_MAX),          // L_TAX_SD     16
    seed(L, 1371272478, O_LCNT_MAX),         // L_SHIP_SD    17
    seed(L, 675466456, O_LCNT_MAX),          // L_SMODE_SD   18
    seed(L, 1808217256, O_LCNT_MAX),         // L_PKEY_SD    19
    seed(L, 2095021727, O_LCNT_MAX),         // L_SKEY_SD    20
    seed(L, 1769349045, O_LCNT_MAX),         // L_SDTE_SD    21
    seed(L, 904914315, O_LCNT_MAX),          // L_CDTE_SD    22
    seed(L, 373135028, O_LCNT_MAX),          // L_RDTE_SD    23
    seed(L, 717419739, O_LCNT_MAX),          // L_RFLG_SD    24
    seed(L, 1095462486, O_LCNT_MAX * 2),     // L_CMNT_SD    25
    seed(C, 881155353, 9),                   // C_ADDR_SD    26
    seed(C, 1489529863, 1),                  // C_NTRG_SD    27
    seed(C, 1521138112, 3),                  // C_PHNE_SD    28
    seed(C, 298370230, 1),                   // C_ABAL_SD    29
    seed(C, 1140279430, 1),                  // C_MSEG_SD    30
    seed(C, 1335826707, 2),                  // C_CMNT_SD    31
    seed(S, 706178559, 9),                   // S_ADDR_SD    32
    seed(S, 110356601, 1),                   // S_NTRG_SD    33
    seed(S, 884434366, 3),                   // S_PHNE_SD    34
    seed(S, 962338209, 1),                   // S_ABAL_SD    35
    seed(S, 1341315363, 2),                  // S_CMNT_SD    36
    seed(P, 709314158, 92),                  // P_NAME_SD    37
    seed(O, 591449447, 1),                   // O_PRIO_SD    38
    seed(L, 431918286, 1),                   // HVAR_SD      39
    seed(O, 851767375, 1),                   // O_CKEY_SD    40
    seed(NATION as i64, 606179079, 2),       // N_CMNT_SD    41
    seed(REGION as i64, 1500869201, 2),      // R_CMNT_SD    42
    seed(O, 1434868289, 1),                  // O_LCNT_SD    43
    seed(S, 263032577, 1),                   // BBB offset   44
    seed(S, 753643799, 1),                   // BBB type     45
    seed(S, 202794285, 1),                   // BBB comment  46
    seed(S, 715851524, 1),                   // BBB junk     47
];

const fn tdef(base: DssHuge, child: i64) -> Tdef {
    Tdef { base, child }
}

/// The initial table definitions (`DBGenContext::tdefs`).
pub const INITIAL_TDEFS: [Tdef; 10] = [
    tdef(200000, PS),   // part
    tdef(200000, NONE), // partsupp
    tdef(10000, NONE),  // supplier
    tdef(150000, NONE), // customer
    tdef(150000, L),    // orders
    tdef(150000, NONE), // lineitem
    tdef(150000, L),    // orders/lineitem
    tdef(200000, PS),   // part/partsupplier
    tdef(NATIONS_MAX, NONE),
    tdef(NATIONS_MAX, NONE),
];

/// The state of one generator (`DBGenContext`).
#[derive(Debug, Clone)]
pub struct DBGenContext {
    pub seed: [Seed; MAX_STREAM + 1],
    pub tdefs: [Tdef; 10],
    pub scale_factor: i64,
    pub permute: Vec<i64>,
}

impl DBGenContext {
    /// `DBGenContext::dM`
    pub const DM: f64 = 2147483647.0;

    pub fn new() -> Self {
        Self {
            seed: INITIAL_SEEDS,
            tdefs: INITIAL_TDEFS,
            scale_factor: 1,
            permute: Vec::new(),
        }
    }
}

impl Default for DBGenContext {
    fn default() -> Self {
        Self::new()
    }
}
