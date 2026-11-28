//! The expected outputs are DuckDB 1.5.6's (`duckdb --batch --no-init`, stdin not a terminal, so a
//! render width of 120 unless `.maxwidth` says otherwise), with the colours stripped.

use super::*;

fn col(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        ty: ty.to_string(),
        ..ColumnInfo::default()
    }
}

fn not_null(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo {
        is_not_null: true,
        ..col(name, ty)
    }
}

/// A table as `.tables` fills it.
fn table(database: &str, schema: &str, name: &str, estimated_size: Option<u64>, columns: Vec<ColumnInfo>) -> TableInfo {
    TableInfo {
        database_name: database.to_string(),
        schema_name: schema.to_string(),
        table_name: name.to_string(),
        estimated_size,
        is_view: false,
        columns,
    }
}

fn main_table(name: &str, columns: Vec<ColumnInfo>) -> TableInfo {
    table("memory", "main", name, Some(0), columns)
}

/// A table as `DESCRIBE` fills it.
fn describe(name: &str, columns: Vec<ColumnInfo>) -> TableInfo {
    table("", "", name, None, columns)
}

fn render(max_width: usize, tables: &[TableInfo]) -> String {
    render_table_metadata(
        &MetadataConfig {
            max_width,
            compact: false,
        },
        tables,
    )
}

fn cols(spec: &[(&str, &str)]) -> Vec<ColumnInfo> {
    spec.iter().map(|(name, ty)| col(name, ty)).collect()
}

fn tpch_customer() -> Vec<ColumnInfo> {
    cols(&[
        ("c_custkey", "BIGINT"),
        ("c_name", "VARCHAR"),
        ("c_address", "VARCHAR"),
        ("c_nationkey", "INTEGER"),
        ("c_phone", "VARCHAR"),
        ("c_acctbal", "DECIMAL(15,2)"),
        ("c_mktsegment", "VARCHAR"),
        ("c_comment", "VARCHAR"),
    ])
}

fn tpch_lineitem() -> Vec<ColumnInfo> {
    cols(&[
        ("l_orderkey", "BIGINT"),
        ("l_partkey", "BIGINT"),
        ("l_suppkey", "BIGINT"),
        ("l_linenumber", "BIGINT"),
        ("l_quantity", "DECIMAL(15,2)"),
        ("l_extendedprice", "DECIMAL(15,2)"),
        ("l_discount", "DECIMAL(15,2)"),
        ("l_tax", "DECIMAL(15,2)"),
        ("l_returnflag", "VARCHAR"),
        ("l_linestatus", "VARCHAR"),
        ("l_shipdate", "DATE"),
        ("l_commitdate", "DATE"),
        ("l_receiptdate", "DATE"),
        ("l_shipinstruct", "VARCHAR"),
        ("l_shipmode", "VARCHAR"),
        ("l_comment", "VARCHAR"),
    ])
}

fn tpch_nation() -> Vec<ColumnInfo> {
    cols(&[
        ("n_nationkey", "INTEGER"),
        ("n_name", "VARCHAR"),
        ("n_regionkey", "INTEGER"),
        ("n_comment", "VARCHAR"),
    ])
}

fn tpch_orders() -> Vec<ColumnInfo> {
    cols(&[
        ("o_orderkey", "BIGINT"),
        ("o_custkey", "BIGINT"),
        ("o_orderstatus", "VARCHAR"),
        ("o_totalprice", "DECIMAL(15,2)"),
        ("o_orderdate", "DATE"),
        ("o_orderpriority", "VARCHAR"),
        ("o_clerk", "VARCHAR"),
        ("o_shippriority", "INTEGER"),
        ("o_comment", "VARCHAR"),
    ])
}

fn tpch_part() -> Vec<ColumnInfo> {
    cols(&[
        ("p_partkey", "BIGINT"),
        ("p_name", "VARCHAR"),
        ("p_mfgr", "VARCHAR"),
        ("p_brand", "VARCHAR"),
        ("p_type", "VARCHAR"),
        ("p_size", "INTEGER"),
        ("p_container", "VARCHAR"),
        ("p_retailprice", "DECIMAL(15,2)"),
        ("p_comment", "VARCHAR"),
    ])
}

fn tpch_partsupp() -> Vec<ColumnInfo> {
    cols(&[
        ("ps_partkey", "BIGINT"),
        ("ps_suppkey", "BIGINT"),
        ("ps_availqty", "BIGINT"),
        ("ps_supplycost", "DECIMAL(15,2)"),
        ("ps_comment", "VARCHAR"),
    ])
}

fn tpch_region() -> Vec<ColumnInfo> {
    cols(&[
        ("r_regionkey", "INTEGER"),
        ("r_name", "VARCHAR"),
        ("r_comment", "VARCHAR"),
    ])
}

fn tpch_supplier() -> Vec<ColumnInfo> {
    cols(&[
        ("s_suppkey", "BIGINT"),
        ("s_name", "VARCHAR"),
        ("s_address", "VARCHAR"),
        ("s_nationkey", "INTEGER"),
        ("s_phone", "VARCHAR"),
        ("s_acctbal", "DECIMAL(15,2)"),
        ("s_comment", "VARCHAR"),
    ])
}

/// The TPC-H tables in the order DuckDB's query returned them (C++ sorts ties in `orders` and `part`
/// unstably; the order here gives the same result with the stable sort).
fn tpch_tables(orders_first: bool) -> Vec<TableInfo> {
    let mut tables = vec![
        main_table("nation", tpch_nation()),
        main_table("region", tpch_region()),
        main_table("partsupp", tpch_partsupp()),
        main_table("lineitem", tpch_lineitem()),
        main_table("customer", tpch_customer()),
        main_table("supplier", tpch_supplier()),
    ];
    if orders_first {
        tables.push(main_table("orders", tpch_orders()));
        tables.push(main_table("part", tpch_part()));
    } else {
        tables.push(main_table("part", tpch_part()));
        tables.push(main_table("orders", tpch_orders()));
    }
    tables
}

const LONG_NAME: &str = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz";

#[test]
fn many_tables_wrap_into_several_displays() {
    assert_eq!(render(120, &tpch_tables(true)), include_str!("tpch_tables.txt"));
}

#[test]
fn many_tables_wide() {
    assert_eq!(render(200, &tpch_tables(false)), include_str!("maxwidth_200.txt"));
}

#[test]
fn describe_with_not_null() {
    let columns = tpch_lineitem()
        .into_iter()
        .map(|c| ColumnInfo { is_not_null: true, ..c })
        .collect();
    assert_eq!(
        render(120, &[describe("lineitem", columns)]),
        include_str!("tpch_describe.txt")
    );
}

#[test]
fn describe_constraints() {
    let columns = vec![
        ColumnInfo {
            is_primary_key: true,
            ..not_null("i", "INTEGER")
        },
        not_null("s", "VARCHAR"),
        ColumnInfo {
            default_value: "1.5".to_string(),
            ..col("d", "DECIMAL(10,2)")
        },
        ColumnInfo {
            is_unique: true,
            ..col("u", "INTEGER")
        },
    ];
    assert_eq!(
        render(120, &[describe("t", columns)]),
        include_str!("describe_constraints.txt")
    );
}

#[test]
fn table_with_primary_key_large_size_and_view() {
    let t = table(
        "memory",
        "main",
        "t",
        Some(1_234_567),
        vec![
            ColumnInfo {
                is_primary_key: true,
                ..col("i", "INTEGER")
            },
            col("s", "VARCHAR"),
            col("d", "DECIMAL(10,2)"),
            col("u", "INTEGER"),
        ],
    );
    let v = TableInfo {
        is_view: true,
        ..table("memory", "main", "v", None, cols(&[("a", "INTEGER")]))
    };
    assert_eq!(render(120, &[v, t]), include_str!("table_and_view.txt"));
}

#[test]
fn databases_and_schemas() {
    let tables = vec![
        table("mydb", "s2", "other_tbl", Some(0), cols(&[("i", "INTEGER")])),
        table("memory", "s1", "my_tbl", Some(0), cols(&[("i", "INTEGER")])),
        TableInfo {
            is_view: true,
            ..table("memory", "main", "v", None, cols(&[("x", "INTEGER")]))
        },
        table(
            "memory",
            "main",
            "t",
            Some(0),
            vec![
                ColumnInfo {
                    is_primary_key: true,
                    ..col("a", "INTEGER")
                },
                col("b", "VARCHAR"),
            ],
        ),
    ];
    assert_eq!(render(120, &tables), include_str!("multi_db.txt"));
}

#[test]
fn long_table_name_is_truncated() {
    let tables = [main_table(LONG_NAME, cols(&[("i", "INTEGER")]))];
    assert_eq!(render(80, &tables), include_str!("long_table.txt"));
}

#[test]
fn long_column_name_is_truncated() {
    let tables = [describe("Describe", cols(&[(LONG_NAME, "INTEGER")]))];
    assert_eq!(render(80, &tables), include_str!("long_column.txt"));
}

#[test]
fn long_type_is_truncated() {
    let ty = format!("STRUCT({LONG_NAME} INTEGER)");
    let tables = [describe("Describe", cols(&[("a", &ty)]))];
    assert_eq!(render(80, &tables), include_str!("long_type.txt"));
}

#[test]
fn long_column_name_and_type_are_truncated() {
    let ty = format!("STRUCT({LONG_NAME} INTEGER)");
    let tables = [describe("Describe", cols(&[(LONG_NAME, &ty)]))];
    assert_eq!(render(80, &tables), include_str!("long_both.txt"));
}

#[test]
fn long_database_name_heading() {
    let name = "thisisanenormousdatabasenamethatmightoverflowthetablerenderingmechanism";
    let tables = [table(name, "main", "tbl", Some(0), cols(&[("i", "INTEGER")]))];
    assert_eq!(render(120, &tables), include_str!("large_db.txt"));
}

fn wide_columns() -> Vec<ColumnInfo> {
    (0..45).map(|i| col(&format!("col{i}"), "INTEGER")).collect()
}

#[test]
fn many_columns_are_split() {
    assert_eq!(
        render(120, &[main_table("wide", wide_columns())]),
        include_str!("split.txt")
    );
    assert_eq!(
        render(120, &[describe("wide", wide_columns())]),
        include_str!("split_describe.txt")
    );
}

#[test]
fn many_columns_too_wide_to_split() {
    // each column of columns is 17 wide: at width 33 only one fits, so the table stays one column
    let out = render(33, &[describe("wide", wide_columns())]);
    assert_eq!(out.lines().count(), 45 + 4);
    assert!(out.starts_with("┌───────────────┐\n│     wide      │\n"));
}

#[test]
fn small_table_is_drawn_next_to_both_big_tables() {
    let tables = [
        main_table(
            "aaa",
            cols(&[
                (
                    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                    "INTEGER",
                ),
                ("b", "INTEGER"),
                ("c", "INTEGER"),
            ]),
        ),
        main_table(
            "bbb",
            cols(&[
                ("yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy", "INTEGER"),
                ("b", "INTEGER"),
            ]),
        ),
        main_table("ccc", cols(&[("z", "INTEGER")])),
    ];
    assert_eq!(render(120, &tables), include_str!("duplicate.txt"));
}

#[test]
fn databases_command() {
    let databases = describe("databases", cols(&[("memory", "(memory)"), ("other", "(memory)")]));
    assert_eq!(render(120, &[databases]), include_str!("databases.txt"));
}

#[test]
fn ties_keep_the_input_order() {
    let tables: Vec<TableInfo> = ["c", "a", "e", "b"]
        .iter()
        .map(|n| main_table(n, cols(&[("x", "INTEGER")])))
        .collect();
    let out = render(120, &tables);
    let names = out.lines().nth(3).unwrap_or_default();
    assert_eq!(names, "│     c     ││     a     ││     e     ││     b     │");
    // all four end on the same line
    assert_eq!(
        out.lines().last(),
        Some("└───────────┘└───────────┘└───────────┘└───────────┘")
    );
}

#[test]
fn compact_mode() {
    let tables = [
        table(
            "memory",
            "main",
            "t",
            Some(1_234_567),
            vec![
                ColumnInfo {
                    is_primary_key: true,
                    ..col("i", "INTEGER")
                },
                col("d", "DECIMAL(10,2)"),
            ],
        ),
        TableInfo {
            is_view: true,
            ..table("memory", "main", "v", None, cols(&[("a", "INTEGER")]))
        },
    ];
    let out = render_table_metadata(
        &MetadataConfig {
            max_width: 120,
            compact: true,
        },
        &tables,
    );
    assert_eq!(
        out,
        "memory.main.t (table, ~1234567 rows): i INTEGER PK, d DECIMAL(10,2)\nmemory.main.v (view): a INTEGER\n"
    );
}

#[test]
fn max_render_width_is_at_least_80() {
    assert_eq!(max_render_width(40), 80);
    assert_eq!(max_render_width(200), 200);
}
