// Ported from duckdb/src/common/box_renderer.cpp @ rust-port-baseline
//
//! The box renderer's output compared with that of the C++ shell (`duckdb --batch`, v1.5.6), with the
//! settings the shell passes in batch mode.

use super::*;

/// The config `ModeDuckBoxRenderer` builds in batch mode (stdout not a console).
fn batch_config(max_width: usize, max_rows: usize) -> BoxRendererConfig {
    BoxRendererConfig {
        max_width: if max_width == 0 { usize::MAX } else { max_width },
        max_rows,
        hidden_rows_hint: "use .last to show entire result".into(),
        ..BoxRendererConfig::default()
    }
}

fn run(config: &BoxRendererConfig, sql: &str) -> String {
    let db = duckrs::Database::open_in_memory().expect("database");
    let mut con = db.connect().expect("connection");
    let result = con.query(sql).expect("query");
    render(config, &result.names, &result.types, &result)
}

fn check(config: &BoxRendererConfig, sql: &str, expected: &str) {
    let actual = run(config, sql);
    assert!(
        actual == expected,
        "for {sql}\nexpected:\n{expected}\nactual:\n{actual}"
    );
}

#[test]
fn range_100() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select * from range(100)"#,
        r#"┌────────────┐
│   range    │
│   int64    │
├────────────┤
│          0 │
│          1 │
│          2 │
│          3 │
│          4 │
│          5 │
│          6 │
│          7 │
│          8 │
│          9 │
│         10 │
│         11 │
│         12 │
│         13 │
│         14 │
│         15 │
│         16 │
│         17 │
│         18 │
│         19 │
│          · │
│          · │
│          · │
│         80 │
│         81 │
│         82 │
│         83 │
│         84 │
│         85 │
│         86 │
│         87 │
│         88 │
│         89 │
│         90 │
│         91 │
│         92 │
│         93 │
│         94 │
│         95 │
│         96 │
│         97 │
│         98 │
│         99 │
└────────────┘
   100 rows 
  (40 shown) 
"#,
    );
}

#[test]
fn mixed_types() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 42 as a, 'hello' as b, NULL::int c, 1.5::decimal(4,1) d, [1,2] l, {'x': 1} s"#,
        r#"┌───────┬─────────┬───────┬──────────────┬─────────┬───────────────────┐
│   a   │    b    │   c   │      d       │    l    │         s         │
│ int32 │ varchar │ int32 │ decimal(4,1) │ int32[] │ struct(x integer) │
├───────┼─────────┼───────┼──────────────┼─────────┼───────────────────┤
│    42 │ hello   │  NULL │          1.5 │ [1, 2]  │ {'x': 1}          │
└───────┴─────────┴───────┴──────────────┴─────────┴───────────────────┘
"#,
    );
}

#[test]
fn empty_result() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select * from range(3) where false"#,
        r#"┌────────┐
│ range  │
│ int64  │
└────────┘
  0 rows
"#,
    );
}

#[test]
fn empty_multi_column() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 1 a, 'x' b from range(3) where false"#,
        r#"┌───────┬─────────┐
│   a   │    b    │
│ int32 │ varchar │
└───────┴─────────┘
      0 rows     
"#,
    );
}

#[test]
fn wide_string() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select repeat('x', 300) a, 1 b"#,
        r#"┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┬───────┐
│                                                                                                                                                      a                                                                                                                                                       │   b   │
│                                                                                                                                                   varchar                                                                                                                                                    │ int32 │
├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┼───────┤
│ xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx │     1 │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┴───────┘
"#,
    );
}

#[test]
fn escaped_newline() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 'a'||chr(10)||'b'||chr(9)||'c' t"#,
        r#"┌─────────┐
│    t    │
│ varchar │
├─────────┤
│ a\nb\tc │
└─────────┘
"#,
    );
}

#[test]
fn nulls() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select * from (values (1, NULL, 'a'), (NULL, 2.5, NULL), (3, -4.25, 'ccc')) t(i, d, s)"#,
        r#"┌───────┬──────────────┬─────────┐
│   i   │      d       │    s    │
│ int32 │ decimal(3,2) │ varchar │
├───────┼──────────────┼─────────┤
│     1 │         NULL │ a       │
│  NULL │         2.50 │ NULL    │
│     3 │        -4.25 │ ccc     │
└───────┴──────────────┴─────────┘
"#,
    );
}

#[test]
fn numbers() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select -1::tinyint a, 200::utinyint b, -32000::smallint c, 123456789012::bigint d, 1.25::double e, 3.5::float f, 12345.678::decimal(10,3) g, 170141183460469231731687303715884105727::hugeint h"#,
        r#"┌──────┬───────┬────────┬──────────────┬────────┬───────┬───────────────┬─────────────────────────────────────────┐
│  a   │   b   │   c    │      d       │   e    │   f   │       g       │                    h                    │
│ int8 │ uint8 │ int16  │    int64     │ double │ float │ decimal(10,3) │                 int128                  │
├──────┼───────┼────────┼──────────────┼────────┼───────┼───────────────┼─────────────────────────────────────────┤
│   -1 │   200 │ -32000 │ 123456789012 │   1.25 │   3.5 │     12345.678 │ 170141183460469231731687303715884105727 │
└──────┴───────┴────────┴──────────────┴────────┴───────┴───────────────┴─────────────────────────────────────────┘
"#,
    );
}

#[test]
fn nested() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select [1,2,3] l, ['a', NULL] ls, {'a': 1, 'b': 'x'} s, map {'k': 1} m, [[1],[2,3]] ll, NULL::int[] nl"#,
        r#"┌───────────┬───────────┬──────────────────────────────┬───────────────────────┬───────────────┬─────────┐
│     l     │    ls     │              s               │           m           │      ll       │   nl    │
│  int32[]  │ varchar[] │ struct(a integer, b varchar) │ map(varchar, integer) │   int32[][]   │ int32[] │
├───────────┼───────────┼──────────────────────────────┼───────────────────────┼───────────────┼─────────┤
│ [1, 2, 3] │ [a, NULL] │ {'a': 1, 'b': x}             │ {k=1}                 │ [[1], [2, 3]] │ NULL    │
└───────────┴───────────┴──────────────────────────────┴───────────────────────┴───────────────┴─────────┘
"#,
    );
}

#[test]
fn other_types() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select true b, date '2024-01-02' d, timestamp '2024-01-02 03:04:05' ts, interval 3 day i, 'abc'::blob bl, '00000000-0000-0000-0000-000000000001'::uuid u"#,
        r#"┌─────────┬────────────┬─────────────────────┬──────────┬──────┬──────────────────────────────────────┐
│    b    │     d      │         ts          │    i     │  bl  │                  u                   │
│ boolean │    date    │      timestamp      │ interval │ blob │                 uuid                 │
├─────────┼────────────┼─────────────────────┼──────────┼──────┼──────────────────────────────────────┤
│ true    │ 2024-01-02 │ 2024-01-02 03:04:05 │ 3 days   │ abc  │ 00000000-0000-0000-0000-000000000001 │
└─────────┴────────────┴─────────────────────┴──────────┴──────┴──────────────────────────────────────┘
"#,
    );
}

#[test]
fn unicode() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 'héllo' a, '日本語' b, 'x' c"#,
        r#"┌─────────┬─────────┬─────────┐
│    a    │    b    │    c    │
│ varchar │ varchar │ varchar │
├─────────┼─────────┼─────────┤
│ héllo   │ 日本語  │ x       │
└─────────┴─────────┴─────────┘
"#,
    );
}

#[test]
fn maxwidth_rows30() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select range a, range*1000000 b, 'abc'||range c, repeat('y', range::int) d from range(30)"#,
        r#"┌───────┬──────────┬─────────┬───────────────────────────────┐
│   a   │    b     │    c    │               d               │
│ int64 │  int64   │ varchar │            varchar            │
├───────┼──────────┼─────────┼───────────────────────────────┤
│     0 │        0 │ abc0    │                               │
│     1 │  1000000 │ abc1    │ y                             │
│     2 │  2000000 │ abc2    │ yy                            │
│     3 │  3000000 │ abc3    │ yyy                           │
│     4 │  4000000 │ abc4    │ yyyy                          │
│     5 │  5000000 │ abc5    │ yyyyy                         │
│     6 │  6000000 │ abc6    │ yyyyyy                        │
│     7 │  7000000 │ abc7    │ yyyyyyy                       │
│     8 │  8000000 │ abc8    │ yyyyyyyy                      │
│     9 │  9000000 │ abc9    │ yyyyyyyyy                     │
│    10 │ 10000000 │ abc10   │ yyyyyyyyyy                    │
│    11 │ 11000000 │ abc11   │ yyyyyyyyyyy                   │
│    12 │ 12000000 │ abc12   │ yyyyyyyyyyyy                  │
│    13 │ 13000000 │ abc13   │ yyyyyyyyyyyyy                 │
│    14 │ 14000000 │ abc14   │ yyyyyyyyyyyyyy                │
│    15 │ 15000000 │ abc15   │ yyyyyyyyyyyyyyy               │
│    16 │ 16000000 │ abc16   │ yyyyyyyyyyyyyyyy              │
│    17 │ 17000000 │ abc17   │ yyyyyyyyyyyyyyyyy             │
│    18 │ 18000000 │ abc18   │ yyyyyyyyyyyyyyyyyy            │
│    19 │ 19000000 │ abc19   │ yyyyyyyyyyyyyyyyyyy           │
│    20 │ 20000000 │ abc20   │ yyyyyyyyyyyyyyyyyyyy          │
│    21 │ 21000000 │ abc21   │ yyyyyyyyyyyyyyyyyyyyy         │
│    22 │ 22000000 │ abc22   │ yyyyyyyyyyyyyyyyyyyyyy        │
│    23 │ 23000000 │ abc23   │ yyyyyyyyyyyyyyyyyyyyyyy       │
│    24 │ 24000000 │ abc24   │ yyyyyyyyyyyyyyyyyyyyyyyy      │
│    25 │ 25000000 │ abc25   │ yyyyyyyyyyyyyyyyyyyyyyyyy     │
│    26 │ 26000000 │ abc26   │ yyyyyyyyyyyyyyyyyyyyyyyyyy    │
│    27 │ 27000000 │ abc27   │ yyyyyyyyyyyyyyyyyyyyyyyyyyy   │
│    28 │ 28000000 │ abc28   │ yyyyyyyyyyyyyyyyyyyyyyyyyyyy  │
│    29 │ 29000000 │ abc29   │ yyyyyyyyyyyyyyyyyyyyyyyyyyyyy │
└───────┴──────────┴─────────┴───────────────────────────────┘
  30 rows                                          4 columns
"#,
    );
}

#[test]
fn many_columns() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select 1 a,2 b,3 c,4 d,5 e,6 f,7 g,8 h,9 i,10 j,11 k,12 l,13 m,14 n,15 o,16 p,17 q,18 r,19 s,20 t,21 u,22 v,23 w,24 x,25 y"#,
        r#"┌───────┬───────┬───────┬───────┬───┬───────┬───────┬───────┬───────┬───────┐
│   a   │   b   │   c   │   d   │ … │   u   │   v   │   w   │   x   │   y   │
│ int32 │ int32 │ int32 │ int32 │ … │ int32 │ int32 │ int32 │ int32 │ int32 │
├───────┼───────┼───────┼───────┼───┼───────┼───────┼───────┼───────┼───────┤
│     1 │     2 │     3 │     4 │ … │    21 │    22 │    23 │    24 │    25 │
└───────┴───────┴───────┴───────┴───┴───────┴───────┴───────┴───────┴───────┘
  1 rows        use .last to show entire result        25 columns (9 shown)
"#,
    );
}

#[test]
fn many_rows_and_columns() {
    let config = batch_config(80, 10);
    check(
        &config,
        r#"select range a, range+1 b, range+2 c, range+3 d, range+4 e, range+5 f, range+6 g, range+7 h, range+8 i, range+9 j, range+10 k, range+11 l from range(1000)"#,
        r#"┌───────┬───────┬───────┬───────┬───────┬───┬───────┬───────┬───────┬───────┐
│   a   │   b   │   c   │   d   │   e   │ … │   i   │   j   │   k   │   l   │
│ int64 │ int64 │ int64 │ int64 │ int64 │ … │ int64 │ int64 │ int64 │ int64 │
├───────┼───────┼───────┼───────┼───────┼───┼───────┼───────┼───────┼───────┤
│     0 │     1 │     2 │     3 │     4 │ … │     8 │     9 │    10 │    11 │
│     1 │     2 │     3 │     4 │     5 │ … │     9 │    10 │    11 │    12 │
│     2 │     3 │     4 │     5 │     6 │ … │    10 │    11 │    12 │    13 │
│     3 │     4 │     5 │     6 │     7 │ … │    11 │    12 │    13 │    14 │
│     4 │     5 │     6 │     7 │     8 │ … │    12 │    13 │    14 │    15 │
│     · │     · │     · │     · │     · │ … │     · │     · │     · │     · │
│     · │     · │     · │     · │     · │ … │     · │     · │     · │     · │
│     · │     · │     · │     · │     · │ … │     · │     · │     · │     · │
│   995 │   996 │   997 │   998 │   999 │ … │  1003 │  1004 │  1005 │  1006 │
│   996 │   997 │   998 │   999 │  1000 │ … │  1004 │  1005 │  1006 │  1007 │
│   997 │   998 │   999 │  1000 │  1001 │ … │  1005 │  1006 │  1007 │  1008 │
│   998 │   999 │  1000 │  1001 │  1002 │ … │  1006 │  1007 │  1008 │  1009 │
│   999 │  1000 │  1001 │  1002 │  1003 │ … │  1007 │  1008 │  1009 │  1010 │
└───────┴───────┴───────┴───────┴───────┴───┴───────┴───────┴───────┴───────┘
  1000 rows (10 shown)                                 12 columns (9 shown)
"#,
    );
}

#[test]
fn hidden_rows_two_columns() {
    let config = batch_config(0, 10);
    check(
        &config,
        r#"select range a, 'v'||range b from range(1000)"#,
        r#"┌───────┬─────────┐
│   a   │    b    │
│ int64 │ varchar │
├───────┼─────────┤
│     0 │ v0      │
│     1 │ v1      │
│     2 │ v2      │
│     3 │ v3      │
│     4 │ v4      │
│     · │ ·       │
│     · │ ·       │
│     · │ ·       │
│   995 │ v995    │
│   996 │ v996    │
│   997 │ v997    │
│   998 │ v998    │
│   999 │ v999    │
└───────┴─────────┘
     1000 rows   
    (10 shown)    
"#,
    );
}

#[test]
fn shortened_expand() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select range a, repeat('abcdefghij', 4+range::int) b, repeat('zyxw', 30) c from range(3)"#,
        r#"┌───────┬───────────────────────────────────┬──────────────────────────────────┐
│   a   │                 b                 │                c                 │
│ int64 │              varchar              │             varchar              │
├───────┼───────────────────────────────────┼──────────────────────────────────┤
│     0 │ abcdefghijabcdefghijabcdefghijabc │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │ defghij                           │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxw         │
├───────┼───────────────────────────────────┼──────────────────────────────────┤
│     1 │ abcdefghijabcdefghijabcdefghijabc │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │ defghijabcdefghij                 │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxw         │
├───────┼───────────────────────────────────┼──────────────────────────────────┤
│     2 │ abcdefghijabcdefghijabcdefghijabc │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │ defghijabcdefghijabcdefghij       │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyxw │
│       │                                   │ zyxwzyxwzyxwzyxwzyxwzyxw         │
└───────┴───────────────────────────────────┴──────────────────────────────────┘
"#,
    );
}

#[test]
fn shortened_many_rows() {
    let config = batch_config(80, 20);
    check(
        &config,
        r#"select range a, repeat('abcdefghij', 4) b, repeat('zyxw', 30) c from range(18)"#,
        r#"┌───────┬───────────────────────────────────┬──────────────────────────────────┐
│   a   │                 b                 │                c                 │
│ int64 │              varchar              │             varchar              │
├───────┼───────────────────────────────────┼──────────────────────────────────┤
│     0 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     1 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     2 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     3 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     4 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     5 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     6 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     7 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     8 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│     9 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    10 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    11 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    12 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    13 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    14 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    15 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    16 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
│    17 │ abcdefghijabcdefghijabcdefghijab… │ zyxwzyxwzyxwzyxwzyxwzyxwzyxwzyx… │
└───────┴───────────────────────────────────┴──────────────────────────────────┘
  18 rows                                                            3 columns
"#,
    );
}

#[test]
fn pretty_struct() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select {'first_name': 'alexander', 'last_name': 'hamilton', 'numbers': [1,2,3,4,5,6,7,8,9,10], 'address': {'street': 'some long street name', 'city': 'new york city'}} s, repeat('q', 70) t"#,
        r#"┌──────────────────────────────────────┬───────────────────────────────────────┐
│                  s                   │                   t                   │
│ struct(first_name varchar, last_name │                varchar                │
│  varchar, numbers integer[], address │                                       │
│  struct(street varchar, city varchar │                                       │
│                  ))                  │                                       │
├──────────────────────────────────────┼───────────────────────────────────────┤
│ {                                    │ qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq │
│   'first_name': alexander,           │ qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq     │
│   'last_name': hamilton,             │                                       │
│   'numbers': [1, 2, 3, 4, 5, 6, 7,   │                                       │
│    8, 9, 10],                        │                                       │
│   'address': {                       │                                       │
│     'street': some long street name, │                                       │
│     'city': new york city            │                                       │
│   }                                  │                                       │
│ }                                    │                                       │
└──────────────────────────────────────┴───────────────────────────────────────┘
"#,
    );
}

#[test]
fn pretty_list() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select [repeat('a', 10), repeat('b', 10), repeat('c', 10), repeat('d', 10), repeat('e', 10), repeat('f', 10)] l, repeat('q', 70) t"#,
        r#"┌──────────────────────────────────────┬───────────────────────────────────────┐
│                  l                   │                   t                   │
│              varchar[]               │                varchar                │
├──────────────────────────────────────┼───────────────────────────────────────┤
│ [aaaaaaaaaa, bbbbbbbbbb, cccccccccc, │ qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq │
│  dddddddddd, eeeeeeeeee, ffffffffff] │ qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq     │
└──────────────────────────────────────┴───────────────────────────────────────┘
"#,
    );
}

#[test]
fn columns_mode() {
    let mut config = batch_config(0, 40);
    config.render_mode = RenderMode::Columns;
    check(
        &config,
        r#"select 1 a, 'x' b, NULL::int c"#,
        r#"┌────────┬─────────┬───────┐
│ Column │  Type   │ Row 1 │
├────────┼─────────┼───────┤
│ a      │ int32   │     1 │
│ b      │ varchar │     x │
│ c      │ int32   │  NULL │
└────────┴─────────┴───────┘
"#,
    );
}

#[test]
fn columns_mode_rows() {
    let mut config = batch_config(0, 40);
    config.render_mode = RenderMode::Columns;
    check(
        &config,
        r#"select range a, 'v'||range b from range(5)"#,
        r#"┌────────┬─────────┬───────┬───────┬───────┬───────┬───────┐
│ Column │  Type   │ Row 1 │ Row 2 │ Row 3 │ Row 4 │ Row 5 │
├────────┼─────────┼───────┼───────┼───────┼───────┼───────┤
│ a      │ int64   │     0 │     1 │     2 │     3 │     4 │
│ b      │ varchar │    v0 │    v1 │    v2 │    v3 │    v4 │
└────────┴─────────┴───────┴───────┴───────┴───────┴───────┘
"#,
    );
}

#[test]
fn columns_mode_hidden() {
    let mut config = batch_config(80, 40);
    config.render_mode = RenderMode::Columns;
    check(
        &config,
        r#"select range a, 'v'||range b from range(100)"#,
        r#"┌────────┬─────────┬───────┬───────┬───┬────────┬────────┬────────┬─────────┐
│ Column │  Type   │ Row 1 │ Row 2 │ … │ Row 97 │ Row 98 │ Row 99 │ Row 100 │
├────────┼─────────┼───────┼───────┼───┼────────┼────────┼────────┼─────────┤
│ a      │ int64   │     0 │     1 │ … │     96 │     97 │     98 │      99 │
│ b      │ varchar │    v0 │    v1 │ … │    v96 │    v97 │    v98 │     v99 │
└────────┴─────────┴───────┴───────┴───┴────────┴────────┴────────┴─────────┘
  100 rows (6 shown)       use .last to show entire result        2 columns
"#,
    );
}

#[test]
fn large_number_footer() {
    let mut config = batch_config(0, 40);
    config.large_number_rendering = LargeNumberRendering::Footer;
    check(
        &config,
        r#"select 123456789 a, 9876543210::bigint b"#,
        r#"┌──────────────────┬────────────────┐
│        a         │       b        │
│      int32       │     int64      │
├──────────────────┼────────────────┤
│    123456789     │   9876543210   │
│ (123.46 million) │ (9.88 billion) │
└──────────────────┴────────────────┘
"#,
    );
}

#[test]
fn large_number_footer_rows() {
    let mut config = batch_config(0, 40);
    config.large_number_rendering = LargeNumberRendering::Footer;
    check(
        &config,
        r#"select * from range(2000000)"#,
        r#"┌───────────────────┐
│       range       │
│       int64       │
├───────────────────┤
│                 0 │
│                 1 │
│                 2 │
│                 3 │
│                 4 │
│                 5 │
│                 6 │
│                 7 │
│                 8 │
│                 9 │
│                10 │
│                11 │
│                12 │
│                13 │
│                14 │
│                15 │
│                16 │
│                17 │
│                18 │
│                19 │
│                 · │
│                 · │
│                 · │
│           1999980 │
│           1999981 │
│           1999982 │
│           1999983 │
│           1999984 │
│           1999985 │
│           1999986 │
│           1999987 │
│           1999988 │
│           1999989 │
│           1999990 │
│           1999991 │
│           1999992 │
│           1999993 │
│           1999994 │
│           1999995 │
│           1999996 │
│           1999997 │
│           1999998 │
│           1999999 │
└───────────────────┘
  2.00 million rows
     (40 shown)     
   (2000000 total)  
"#,
    );
}

#[test]
fn large_number_all() {
    let mut config = batch_config(0, 40);
    config.large_number_rendering = LargeNumberRendering::All;
    check(
        &config,
        r#"select 123456789 a, 42 b, 1500000.5::double c"#,
        r#"┌────────────────┬───────┬──────────────┐
│       a        │   b   │      c       │
│     int32      │ int32 │    double    │
├────────────────┼───────┼──────────────┤
│ 123.46 million │    42 │ 1.50 million │
└────────────────┴───────┴──────────────┘
"#,
    );
}

#[test]
fn thousand_sep() {
    let mut config = batch_config(0, 40);
    config.thousand_separator = Some(',');
    config.decimal_separator = Some('.');
    check(
        &config,
        r#"select 1234567 a, 1234.5::decimal(10,2) b, -1234567 c, 'x1234' d"#,
        r#"┌───────────┬───────────────┬──────────┬─────────┐
│     a     │       b       │    c     │    d    │
│   int32   │ decimal(10,2) │  int32   │ varchar │
├───────────┼───────────────┼──────────┼─────────┤
│ 1,234,567 │      1,234.50 │ -1234567 │ x1234   │
└───────────┴───────────────┴──────────┴─────────┘
"#,
    );
}

#[test]
fn null_value() {
    let mut config = batch_config(0, 40);
    config.null_value = "(null)".into();
    check(
        &config,
        r#"select NULL::int a, 1 b"#,
        r#"┌────────┬───────┐
│   a    │   b   │
│ int32  │ int32 │
├────────┼───────┤
│ (null) │     1 │
└────────┴───────┘
"#,
    );
}

#[test]
fn width_100_mixed() {
    let config = batch_config(100, 40);
    check(
        &config,
        r#"select range id, 'name_'||range||repeat('n', range::int*3) nm, range*1.5 dbl, [range, range+1] lst, {'k': repeat('v', range::int*4)} st, repeat('t', 50) txt from range(6)"#,
        r#"┌───────┬───────────────────────┬───────────────┬───┬───────────────────────┬──────────────────────┐
│  id   │          nm           │      dbl      │ … │          st           │         txt          │
│ int64 │        varchar        │ decimal(21,1) │ … │   struct(k varchar)   │       varchar        │
├───────┼───────────────────────┼───────────────┼───┼───────────────────────┼──────────────────────┤
│     0 │ name_0                │           0.0 │ … │ {'k': ''}             │ ttttttttttttttttttt… │
│     1 │ name_1nnn             │           1.5 │ … │ {'k': vvvv}           │ ttttttttttttttttttt… │
│     2 │ name_2nnnnnn          │           3.0 │ … │ {'k': vvvvvvvv}       │ ttttttttttttttttttt… │
│     3 │ name_3nnnnnnnnn       │           4.5 │ … │ {'k': vvvvvvvvvvvv}   │ ttttttttttttttttttt… │
│     4 │ name_4nnnnnnnnnnnn    │           6.0 │ … │ {'k': vvvvvvvvvvvvvv… │ ttttttttttttttttttt… │
│     5 │ name_5nnnnnnnnnnnnnnn │           7.5 │ … │ {'k': vvvvvvvvvvvvvv… │ ttttttttttttttttttt… │
└───────┴───────────────────────┴───────────────┴───┴───────────────────────┴──────────────────────┘
  6 rows                    use .last to show entire result                    6 columns (5 shown)
"#,
    );
}

#[test]
fn width_130_mixed() {
    let config = batch_config(130, 40);
    check(
        &config,
        r#"select range id, 'name_'||range||repeat('n', range::int*3) nm, range*1.5 dbl, [range, range+1] lst, {'k': repeat('v', range::int*4)} st, repeat('t', 50) txt from range(6)"#,
        r#"┌───────┬───────────────────────┬───────────────┬─────────┬─────────────────────────────┬────────────────────────────────────────┐
│  id   │          nm           │      dbl      │   lst   │             st              │                  txt                   │
│ int64 │        varchar        │ decimal(21,1) │ int64[] │      struct(k varchar)      │                varchar                 │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     0 │ name_0                │           0.0 │ [0, 1]  │ {'k': ''}                   │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │                             │ tttttttttttt                           │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     1 │ name_1nnn             │           1.5 │ [1, 2]  │ {'k': vvvv}                 │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │                             │ tttttttttttt                           │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     2 │ name_2nnnnnn          │           3.0 │ [2, 3]  │ {'k': vvvvvvvv}             │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │                             │ tttttttttttt                           │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     3 │ name_3nnnnnnnnn       │           4.5 │ [3, 4]  │ {'k': vvvvvvvvvvvv}         │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │                             │ tttttttttttt                           │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     4 │ name_4nnnnnnnnnnnn    │           6.0 │ [4, 5]  │ {'k': vvvvvvvvvvvvvvvv}     │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │                             │ tttttttttttt                           │
├───────┼───────────────────────┼───────────────┼─────────┼─────────────────────────────┼────────────────────────────────────────┤
│     5 │ name_5nnnnnnnnnnnnnnn │           7.5 │ [5, 6]  │ {                           │ tttttttttttttttttttttttttttttttttttttt │
│       │                       │               │         │   'k': vvvvvvvvvvvvvvvvvvvv │ tttttttttttt                           │
│       │                       │               │         │ }                           │                                        │
└───────┴───────────────────────┴───────────────┴─────────┴─────────────────────────────┴────────────────────────────────────────┘
"#,
    );
}

#[test]
fn width_90_hidden_rows() {
    let config = batch_config(90, 12);
    check(
        &config,
        r#"select range id, repeat('w', 30) a, repeat('x', 30) b, repeat('y', 30) c from range(50)"#,
        r#"┌───────┬──────────────────────────┬──────────────────────────┬──────────────────────────┐
│  id   │            a             │            b             │            c             │
│ int64 │         varchar          │         varchar          │         varchar          │
├───────┼──────────────────────────┼──────────────────────────┼──────────────────────────┤
│     0 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     1 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     2 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     3 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     4 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     5 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│     · │            ·             │            ·             │            ·             │
│     · │            ·             │            ·             │            ·             │
│     · │            ·             │            ·             │            ·             │
│    44 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│    45 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│    46 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│    47 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│    48 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
│    49 │ wwwwwwwwwwwwwwwwwwwwwww… │ xxxxxxxxxxxxxxxxxxxxxxx… │ yyyyyyyyyyyyyyyyyyyyyyy… │
└───────┴──────────────────────────┴──────────────────────────┴──────────────────────────┘
  50 rows (12 shown)              use .last to show entire result              4 columns
"#,
    );
}

#[test]
fn unicode_truncate() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select repeat('日本', 50) a, repeat('é', 70) b from range(2)"#,
        r#"┌──────────────────────────────────────┬───────────────────────────────────────┐
│                  a                   │                   b                   │
│               varchar                │                varchar                │
├──────────────────────────────────────┼───────────────────────────────────────┤
│ 日本日本日本日本日本日本日本日本日本 │ ééééééééééééééééééééééééééééééééééééé │
│ 日本日本日本日本日本日本日本日本日本 │ ééééééééééééééééééééééééééééééééé     │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本                 │                                       │
├──────────────────────────────────────┼───────────────────────────────────────┤
│ 日本日本日本日本日本日本日本日本日本 │ ééééééééééééééééééééééééééééééééééééé │
│ 日本日本日本日本日本日本日本日本日本 │ ééééééééééééééééééééééééééééééééé     │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本日本日本日本日本 │                                       │
│ 日本日本日本日本日本                 │                                       │
└──────────────────────────────────────┴───────────────────────────────────────┘
"#,
    );
}

#[test]
fn columns_mode_wide() {
    let mut config = batch_config(80, 40);
    config.render_mode = RenderMode::Columns;
    check(
        &config,
        r#"select repeat('x', 100) a, 42 b, [1,2,3] c from range(3)"#,
        r#"┌────────┬─────────┬───┬────────────────────────────────┬──────────────────────┐
│ Column │  Type   │ … │             Row 2              │        Row 3         │
├────────┼─────────┼───┼────────────────────────────────┼──────────────────────┤
│ a      │ varchar │ … │ xxxxxxxxxxxxxxxxxxxxxxxxxxxxx… │ xxxxxxxxxxxxxxxxxxx… │
│ b      │ int32   │ … │                             42 │                   42 │
│ c      │ int32[] │ … │                      [1, 2, 3] │            [1, 2, 3] │
└────────┴─────────┴───┴────────────────────────────────┴──────────────────────┘
"#,
    );
}

#[test]
fn divider_short_values() {
    let config = batch_config(0, 10);
    check(
        &config,
        r#"select case when range < 5 then 'long value here' else '' end s, case when range < 5 then 12345 else 1 end n from range(20)"#,
        r#"┌─────────────────┬───────┐
│        s        │   n   │
│     varchar     │ int32 │
├─────────────────┼───────┤
│ long value here │ 12345 │
│ long value here │ 12345 │
│ long value here │ 12345 │
│ long value here │ 12345 │
│ long value here │ 12345 │
│        ·        │     · │
│        ·        │     · │
│        ·        │     · │
│                 │     1 │
│                 │     1 │
│                 │     1 │
│                 │     1 │
│                 │     1 │
└─────────────────┴───────┘
  20 rows       2 columns
  (10 shown)              
"#,
    );
}

#[test]
fn json_value() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select '{"a": [1, 2, {"b": null}], "c": "some long string value to wrap", "d": {"e": true, "f": 3.5}}'::json j, repeat('p', 60) p"#,
        r#"┌──────────────────────────────────────┬───────────────────────────────────────┐
│                  j                   │                   p                   │
│                 json                 │                varchar                │
├──────────────────────────────────────┼───────────────────────────────────────┤
│ {                                    │ ppppppppppppppppppppppppppppppppppppp │
│   "a": [1, 2, {"b": null             │ ppppppppppppppppppppppp               │
│    }                                 │                                       │
│   ],                                 │                                       │
│   "c":                               │                                       │
│   "some long string value to wrap",  │                                       │
│   "d": {"e": true, "f": 3.5}         │                                       │
│ }                                    │                                       │
└──────────────────────────────────────┴───────────────────────────────────────┘
"#,
    );
}

#[test]
fn map_and_nested_lists() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select map {'key1': [1,2,3], 'key2': [4,5,6,7,8,9,10,11,12,13,14,15]} m, [[1,2,3],[4,5,6],[7,8,9],[10,11,12]] nl, repeat('z', 50) z"#,
        r#"┌─────────────────────────┬──────────────────────────┬─────────────────────────┐
│            m            │            nl            │            z            │
│ map(varchar, integer[]) │        int32[][]         │         varchar         │
├─────────────────────────┼──────────────────────────┼─────────────────────────┤
│ {                       │ [[1, 2, 3],              │ zzzzzzzzzzzzzzzzzzzzzzz │
│   key1=[1, 2, 3],       │  [4, 5, 6],              │ zzzzzzzzzzzzzzzzzzzzzzz │
│   key2=[4, 5, 6, 7, 8,  │  [7, 8, 9],              │ zzzz                    │
│    9, 10, 11, 12, 13,   │  [10, 11, 12]            │                         │
│    14, 15]              │ ]                        │                         │
│ }                       │                          │                         │
└─────────────────────────┴──────────────────────────┴─────────────────────────┘
"#,
    );
}

#[test]
fn nested_nulls_expand() {
    let config = batch_config(80, 40);
    check(
        &config,
        r#"select * from (values ({'a': NULL::int, 'b': 'null and more nulls'}, repeat('m', 60)), (NULL, 'x')) t(s, m)"#,
        r#"┌───────────────────────────────────────┬──────────────────────────────────────┐
│                   s                   │                  m                   │
│     struct(a integer, b varchar)      │               varchar                │
├───────────────────────────────────────┼──────────────────────────────────────┤
│ {'a': null, 'b': nulland more nulls}  │ mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm │
│                                       │ mmmmmmmmmmmmmmmmmmmmmmmm             │
├───────────────────────────────────────┼──────────────────────────────────────┤
│ NULL                                  │ x                                    │
└───────────────────────────────────────┴──────────────────────────────────────┘
"#,
    );
}

#[test]
fn enum_type() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 'b'::enum('a', 'b''c', 'b') e"#,
        r#"┌────────────────────────┐
│           e            │
│ enum('a', 'b''c', 'b') │
├────────────────────────┤
│ b                      │
└────────────────────────┘
"#,
    );
}

#[test]
fn long_column_name() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select 1 as "a very long column name here""#,
        r#"┌──────────────────────────────┐
│ a very long column name here │
│            int32             │
├──────────────────────────────┤
│                            1 │
└──────────────────────────────┘
"#,
    );
}

#[test]
fn one_row_many() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select * from range(43)"#,
        r#"┌───────┐
│ range │
│ int64 │
├───────┤
│     0 │
│     1 │
│     2 │
│     3 │
│     4 │
│     5 │
│     6 │
│     7 │
│     8 │
│     9 │
│    10 │
│    11 │
│    12 │
│    13 │
│    14 │
│    15 │
│    16 │
│    17 │
│    18 │
│    19 │
│    20 │
│    21 │
│    22 │
│    23 │
│    24 │
│    25 │
│    26 │
│    27 │
│    28 │
│    29 │
│    30 │
│    31 │
│    32 │
│    33 │
│    34 │
│    35 │
│    36 │
│    37 │
│    38 │
│    39 │
│    40 │
│    41 │
│    42 │
└───────┘
"#,
    );
}

#[test]
fn rows_44() {
    let config = batch_config(0, 40);
    check(
        &config,
        r#"select * from range(44)"#,
        r#"┌────────────┐
│   range    │
│   int64    │
├────────────┤
│          0 │
│          1 │
│          2 │
│          3 │
│          4 │
│          5 │
│          6 │
│          7 │
│          8 │
│          9 │
│         10 │
│         11 │
│         12 │
│         13 │
│         14 │
│         15 │
│         16 │
│         17 │
│         18 │
│         19 │
│          · │
│          · │
│          · │
│         24 │
│         25 │
│         26 │
│         27 │
│         28 │
│         29 │
│         30 │
│         31 │
│         32 │
│         33 │
│         34 │
│         35 │
│         36 │
│         37 │
│         38 │
│         39 │
│         40 │
│         41 │
│         42 │
│         43 │
└────────────┘
   44 rows  
  (40 shown) 
"#,
    );
}

/// `max_analyze_rows`: the widths come from the first chunk; later wider values are truncated.
#[test]
fn max_analyze_rows_truncates_later_rows() {
    let mut config = batch_config(0, 5000);
    config.max_analyze_rows = 2048;
    let actual = run(
        &config,
        "select case when range >= 2048 then 'longer value' else 'v' end s, range from range(2100)",
    );
    let lines: Vec<&str> = actual.lines().collect();
    assert_eq!(lines.len(), 2106);
    assert_eq!(
        lines[..5].join("\n"),
        "┌─────────┬───────┐\n│    s    │ range │\n│ varchar │ int64 │\n├─────────┼───────┤\n│ v       │     0 │"
    );
    assert_eq!(lines[2051], "│ v       │  2047 │");
    assert_eq!(lines[2052], "│ longer… │  2048 │");
    assert_eq!(lines[2104], "└─────────┴───────┘");
    assert_eq!(lines[2105], "     2100 rows   ");
}

#[test]
fn large_number_formatting() {
    assert_eq!(try_format_large_number("12345", None), "");
    assert_eq!(try_format_large_number("994999", None), "");
    assert_eq!(try_format_large_number("995000", None), "1.00 million");
    assert_eq!(try_format_large_number("1234567", None), "1.23 million");
    assert_eq!(try_format_large_number("-1234567.89", Some(',')), "-1,23 million");
    assert_eq!(try_format_large_number("9876543210", None), "9.88 billion");
    assert_eq!(try_format_large_number("1.5e10", None), "");
}

#[test]
fn type_names() {
    assert_eq!(render_type(&LogicalType::new(LogicalTypeId::Integer)), "int32");
    assert_eq!(
        render_type(&LogicalType::list(LogicalType::new(LogicalTypeId::UBigInt))),
        "uint64[]"
    );
    assert_eq!(
        render_type(&LogicalType::new(LogicalTypeId::Varchar).with_alias("JSON")),
        "json"
    );
    assert_eq!(
        render_type(&LogicalType::enum_type(vec!["A".into(), "it's".into()])),
        "enum('A', 'it''s')"
    );
}

#[test]
fn render_widths() {
    assert_eq!(unicode::render_width("abc"), 3);
    assert_eq!(unicode::render_width("日本"), 4);
    assert_eq!(unicode::render_width("e\u{301}"), 1);
    assert_eq!(unicode::render_width("👍🏽"), 2);
    assert_eq!(unicode::render_width("👨\u{200d}👩\u{200d}👧"), 2);
}
