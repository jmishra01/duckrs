# How the duckrs optimizer makes queries faster

The binder turns SQL into a logical plan: scans, filters, joins, aggregates. The optimizer rewrites that plan into one that returns exactly the same rows with less work. Stage 3 ported DuckDB's optimizer passes to Rust so that duckrs picks the same plans DuckDB does.

*Branch phase-52-stage3-optimizer · crates/duckrs-core/src/optimizer/ · 48 files, about 22,500 lines added*

## Five ways a pass saves work

Every optimization below uses at least one of these.

- **Read less**: Skip rows, columns, row groups and files that can't matter.
- **Shrink early**: Filter or aggregate before a join, so the join sees fewer rows.
- **Compute once**: Share repeated subplans and reuse aggregate states.
- **Cheaper algorithm**: Better join order, the right join operator, Top-N in place of a full sort.
- **Cheaper values**: Simpler expressions and narrower types for sorting and grouping.

## The pipeline

The passes run in a fixed order, the same as DuckDB's `Optimizer::Optimize` (see `optimizer/mod.rs`). The order matters: expressions get simplified before filters move, filters move before joins are reordered, and physical choices come last, once the statistics are known.

1. **Simplify expressions and aggregates.** Make each expression cheaper and easier for the later passes to understand.
   Passes: `expression_rewriter`, `cte_inlining`, `self_join_aggregate`, `scalar_aggregate_merge`, `aggregate_function_rewriter`, `redundant_order`
2. **Move filters.** Lift filters up, then push each one as far down as it can go. Use statistics to decide filters in advance.
   Passes: `filter_pullup`, `filter_pushdown`, `statistics_propagation (filters)`, `cte_filter_pusher`
3. **Reshape subqueries, joins and windows.** Flatten correlated subqueries, simplify outer joins, and expose the joins to the join orderer.
   Passes: `unnest_rewriter`, `deliminator`, `in_clause`, `grouping_sets`, `disjunctive_join_rewriter`, `cte_inlining`, `empty_result_pullup`, `window_self_join`, `projection_pullup`, `window_join_pushdown`, `outer_join_simplification`
4. **Order joins and aggregates around them.** Pick the cheapest join order from cardinality estimates, then decide where aggregation happens.
   Passes: `join_order`, `aggregate_reuse`, `partial_aggregate_pushdown`, `join_elimination`, `redundant_order_keys`
5. **Statistics and physical choices.** Propagate statistics, then use them to choose Top-N, scan filters, columns, value widths, join operators and indexes.
   Passes: `statistics_propagation`, `sampling_pushdown`, `top_n`, `scan filters`, `unused_columns`, `late_materialization`, `extract pushdown`, `compressed_materialization`, `projection_placement`, `top_n_window_elimination`, `common_aggregate`, `reorder_filter`, `partitioned_execution`, `common_subplan`, `index scan`, `window_rewriter`, `type_pushdown`, `range joins`

## One query through the optimizer

```
SELECT c.name, o.total
FROM orders o
JOIN customers c ON o.cust_id = c.id
WHERE year(o.order_date) = 2025
  AND c.country = 'IN'
ORDER BY o.total DESC
LIMIT 10;
```

**Before: plan from the binder**

```
LIMIT 10
└ ORDER BY total DESC
  └ PROJECTION name, total
    └ FILTER year(order_date) = 2025
             AND country = 'IN'
      └ JOIN o.cust_id = c.id
        ├ SCAN orders     (all columns)
        └ SCAN customers  (all columns)
```

**After: optimized plan**

```
TOP_N 10 BY total DESC
└ PROJECTION name, total
  └ HASH_JOIN o.cust_id = c.id
    ├ SCAN orders [cust_id, total]
    │   order_date >= 2025-01-01
    │   AND order_date <= 2025-12-31
    └ SCAN customers [id, name]  ← build
        country = 'IN'
```

1. **monotone_preimage** (part of the expression rewriter) turns `year(order_date) = 2025` into a plain date range. A range on the column itself can use min/max statistics. A function call on it can't.
2. **filter_pushdown** splits the WHERE clause and sends each condition to the one table it reads, below the join.
3. **scan filters** put those conditions inside the scans, so zone maps can skip whole row groups whose dates fall outside 2025.
4. **join_order** and **build_side** build the hash table on the small, filtered `customers` side. The large `orders` side only probes it.
5. **unused_columns** stops reading every column except the four the query needs, so the join copies narrower rows.
6. **top_n** merges `ORDER BY … LIMIT 10` into one Top-N that keeps 10 rows in a heap and never sorts the full join result.

> These plans are simplified to show the idea. They are not exact EXPLAIN output.

## TPC-H Q1, with the optimizer off and on

Q1 reads one table and has no joins, so none of the join passes apply. It shows what the optimizer does to a large aggregate instead: 8 aggregates over almost all of lineitem, grouped into 4 groups. The plans are from the same build as Q3 (`081d0b9`) and the same SF0.1 data.

```
SELECT l_returnflag, l_linestatus,
       sum(l_quantity) AS sum_qty,
       sum(l_extendedprice) AS sum_base_price,
       sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price,
       sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge,
       avg(l_quantity) AS avg_qty,
       avg(l_extendedprice) AS avg_price,
       avg(l_discount) AS avg_disc,
       count(*) AS count_order
FROM lineitem
WHERE l_shipdate <= CAST('1998-09-02' AS date)
GROUP BY l_returnflag, l_linestatus
ORDER BY l_returnflag, l_linestatus;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By l_returnflag ASC, l_linestatus ASC                 ~5
└ Hash Group By l_returnflag, l_linestatus                  ~5
    sum(l_quantity), sum(l_extendedprice), sum(disc_price), sum(charge),
    avg(l_quantity), avg(l_extendedprice), avg(l_discount), count_star()
  └ Filter  l_shipdate <= '1998-09-02'            ~120,114
    └ Seq Scan lineitem  (all 16 columns)            ~600,572
```

**Optimizer on: default settings**

```
Projection __internal_decompress_string(l_returnflag), __internal_decompress_string(l_linestatus), ...   ~5
└ Order By l_returnflag ASC, l_linestatus ASC               ~5
  └ Projection __internal_compress_string_utinyint(l_returnflag), (l_linestatus), ...
    └ Projection l_returnflag, l_linestatus, sum_qty, ..., count_order
      └ Projection ..., CAST(sum AS DOUBLE) / CAST(count AS DOUBLE)  (each of the 3 averages)
        └ Hash Group By l_returnflag, l_linestatus          ~5
            sum(l_quantity), sum(l_extendedprice), sum(disc_price), sum(charge),
            sum(l_quantity), sum(l_extendedprice), sum(l_discount),
            count_star(), count_star(), count_star(), count_star()
          └ Projection l_quantity, l_extendedprice, l_discount, l_tax,
                       __internal_compress_string_utinyint(l_returnflag),
                       __internal_compress_string_utinyint(l_linestatus)   ~120,114
            └ Seq Scan lineitem  [l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus]   ~120,114
                Filters: l_shipdate <= '1998-09-02'
```

> In the optimized plan, the second `sum(l_quantity)` and `sum(l_extendedprice)` and three of the four `count_star()` compute the same thing as another aggregate in the list.

### What each pass changed

1. **filter_pushdown** moved the ship date filter into the lineitem scan. The estimate of ~120,114 rows is the default guess of 20% for a comparison. In fact 591,856 of the 600,572 rows (98.5%) pass. The column's min/max range includes the cut-off date, so statistics can neither drop the filter nor skip row groups.
2. **unused_columns** cut lineitem from 16 columns to 7: the six the aggregate reads, plus `l_shipdate`, which the scan reads for its filter and doesn't output.
3. **aggregate_function_rewriter** turned the three `avg(x)`s into `sum(x) / count(x)`, computed in a projection above the GROUP BY. All the columns are NOT NULL, so each `count(x)` became `count_star()`. The aggregate list still has duplicates, though. `sum(l_quantity)` and `sum(l_extendedprice)` appear twice and `count_star()` four times, so 11 aggregates compute only 7 different values: one more state per group than the 8 aggregates written in the query. DuckDB 1.5.6 doesn't rewrite Q1's averages at all.
4. **compressed_materialization** packs the one-letter `l_returnflag` and `l_linestatus` strings into one-byte integers (`utinyint`), below the GROUP BY and again below the ORDER BY. The top projection unpacks them. Grouping and sorting then compare bytes instead of strings.
5. **Perfect hash group-by doesn't kick in.** DuckDB's plan for Q1 uses `PERFECT_HASH_GROUP_BY`, which looks the two packed bytes up directly in a small array instead of hashing them. In duckrs, statistics propagation decides on the perfect hash before compression runs (`statistics.rs`, `perfect_hash_bits`), and at that point the groups are still VARCHAR, which it only accepts as integers. The compression pass doesn't revisit the decision, so duckrs uses an ordinary hash group-by. In C++, compression happens inside statistics propagation, so the aggregate already sees integer groups.

- **4 rows**: The result: one row per return flag and line status (A F, N F, N O, R F).
- **98.5%**: Rows that pass the filter (591,856 of 600,572), against an estimate of 20%. With no joins, the wrong estimate doesn't change the plan here.
- **11 for 7**: Aggregates in the GROUP BY against the different values they compute. I couldn't measure their cost: the machine's load average was above 70 during these runs, so Q1's timings (about 0.01–0.02 s) were too noisy to compare.

> **Fixed, merged into phase-55-optimizer-rewriters (a74af3f, merge 0426536):** a port of `CommonAggregateOptimizer` (`COMMON_AGGREGATE`) now runs where C++ runs it, after the statistics, and merges identical aggregates. Q1's GROUP BY drops from 11 aggregates to 6. Compressed GROUP BY columns now carry the value range of their compressed form, as C++'s `UpdateAggregateStats` does, and the perfect hash decision is made again from those ranges. Q1's plan now shows **Perfect Hash Group By**, as DuckDB's does. In duckrs that operator is only a name over the same hash table, so this second fix makes the plans match but doesn't make Q1 faster. The plans above are from before the fix.

## TPC-H Q3, with the optimizer off and on

Q3 is the simplest of the join queries here. It finds the 10 unshipped orders with the highest revenue for the BUILDING market segment, from three tables with one filter each. The plans are from a release build of `phase-55-optimizer-rewriters` at `081d0b9`, on the same TPC-H SF0.1 data as the other walkthroughs.

```
SELECT l_orderkey,
       sum(l_extendedprice * (1 - l_discount)) AS revenue,
       o_orderdate, o_shippriority
FROM customer, orders, lineitem
WHERE c_mktsegment = 'BUILDING'
  AND c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND o_orderdate < CAST('1995-03-15' AS date)
  AND l_shipdate  > CAST('1995-03-15' AS date)
GROUP BY l_orderkey, o_orderdate, o_shippriority
ORDER BY revenue DESC, o_orderdate
LIMIT 10;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Top N 10 BY revenue DESC, o_orderdate ASC                   ~10
└ Projection l_orderkey, revenue, o_orderdate, o_shippriority   ~758,569,228
  └ Hash Group By l_orderkey, o_orderdate, o_shippriority, sum(...)   ~758,569,228
    └ Filter (5 conditions)                   ~270,257,400,000,000
      └ Cross Product                         ~1,351,287,000,000,000
        ├ Cross Product                       ~2,250,000,000
        │ ├ Seq Scan customer  (all 8 columns)              ~15,000
        │ └ Seq Scan orders    (all 9 columns)              ~150,000
        └ Seq Scan lineitem    (all 16 columns)             ~600,572
```

**Optimizer on: default settings**

```
Top N 10 BY revenue DESC, o_orderdate ASC                   ~10
└ Projection l_orderkey, revenue, o_orderdate, o_shippriority   ~3,219
  └ Projection CAST(l_orderkey AS BIGINT), o_orderdate, CAST(o_shippriority AS INTEGER), revenue
    └ Hash Group By l_orderkey, o_orderdate, o_shippriority,
                    sum(l_extendedprice * (1.00 - l_discount))   ~3,219
      └ Projection CAST(l_orderkey AS INTEGER), ..., CAST(o_shippriority AS TINYINT)
        └ Hash Join  l_orderkey = o_orderkey              ~3,219
          ├ Seq Scan lineitem  [l_orderkey, l_extendedprice, l_discount]   ~120,114
          │   Filters: l_shipdate > '1995-03-15'
          └ Hash Join  o_custkey = c_custkey              ~8,015
            ├ Seq Scan orders  [o_orderkey, o_custkey, o_orderdate, o_shippriority]   ~30,000
            │   Filters: o_orderdate < '1995-03-15'
            └ Seq Scan customer  [c_custkey]              ~3,750
                Filters: c_custkey <= 14999 AND c_mktsegment = 'BUILDING'
```

> The first child of each hash join probes and the second child is the build side.

### What each pass changed

1. **filter_pushdown** sent each of the three single-table conditions into its own scan: the market segment into customer, the order date into orders and the ship date into lineitem. The two join conditions turned the cross products into hash joins. Without this, every row of the 1.35 × 10^15-row cross product would go through the filter.
2. **join_order** starts from the most selective table. BUILDING customers (~3,750) are the build side for orders before 15 March 1995 (~30,000). Their join (~8,015) is in turn the build side for lineitem (~120,114 after its ship date filter). lineitem, the largest table, only probes.
3. **statistics_propagation** added `c_custkey <= 14999` to the customer scan. This is the same bound as in Q5 and Q18, from the largest `o_custkey` in orders.
4. **unused_columns** cut lineitem to 3 of its 16 columns, orders to 4 of 9 and customer to 1 of 8. The filter columns `l_shipdate` and `c_mktsegment` are read inside their scans and go no further.
5. **compressed_materialization** narrows the GROUP BY keys. `l_orderkey` fits in an INTEGER, and `o_shippriority` in a TINYINT, since every value is 0 at this scale. They're widened back after the aggregate. The casts disappear with `SET disabled_optimizers = 'compressed_materialization'`.
6. **top_n** keeps only 10 of the ~3,219 groups in a heap, so the groups are never fully sorted. It appears in both plans because it has no switch. Late materialization doesn't apply here: it needs a scan right under the Top-N, and this one sits on an aggregate.

- **0.010 s**: Q3 with the optimizer on, median of 7 warm runs. It returns 10 rows, led by order 223140.
- **Not run**: The plan with the optimizer off. Its cross product of the three tables is estimated at 1.35 × 10^15 rows.
- **0.011 s**: Q3 with `SET disabled_optimizers = 'join_filter_pushdown'`. The runtime join filters that hash joins pass to their probe scans don't appear in EXPLAIN, and at this size they barely change the time.

## TPC-H Q4, with the optimizer off and on

Q4 counts the orders of one quarter that had at least one late line item, by priority. It is the only walkthrough with a correlated subquery: an `EXISTS` that reads `o_orderkey` from the outer query. It shows how a subquery that would run once per order becomes a single join. The plans are from the same build as Q7 and Q8 (`1ed6cc1`) and the same SF0.1 data.

```
SELECT o_orderpriority, count(*) AS order_count
FROM orders
WHERE o_orderdate >= CAST('1993-07-01' AS date)
  AND o_orderdate <  CAST('1993-10-01' AS date)
  AND EXISTS (SELECT * FROM lineitem
              WHERE l_orderkey = o_orderkey
                AND l_commitdate < l_receiptdate)
GROUP BY o_orderpriority
ORDER BY o_orderpriority;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By o_orderpriority                                    ~5
└ Hash Group By o_orderpriority, count_star()               ~5
  └ Filter  o_orderdate (2 conditions) AND #mark         ~30,000
    └ Left Delim Join  MARK  o_orderkey IS NOT DISTINCT FROM o_orderkey   ~150,000
      ├ Seq Scan orders  (all 9 columns)                 ~150,000
      └ Filter  l_orderkey = o_orderkey AND l_commitdate < l_receiptdate   ~18,017,160,000
        └ Cross Product                           ~90,085,800,000
          ├ Seq Scan lineitem  (all 16 columns)         ~600,572
          └ CTE Scan  (the distinct o_orderkey values of orders)   ~150,000
```

> The planner flattens the subquery before any optimizer pass runs, as DuckDB's does. The **Delim Join** collects the distinct outer keys (`o_orderkey`), the subquery is joined to them, and a MARK join tells each order whether it found a match. With the optimizer off, that inner join is still a cross product.

**Optimizer on: default settings**

```
Projection ...                                              ~5
└ Order By o_orderpriority                                  ~5
  └ Hash Group By o_orderpriority, count_star()             ~5
    └ Hash Join  RIGHT_SEMI  o_orderkey = o_orderkey     ~6,000
      ├ Projection NULL::BIGINT, NULL::BIGINT, ... (16 NULL columns), o_orderkey   ~120,114
      │ └ Filter  l_commitdate < l_receiptdate          ~120,114
      │   └ Seq Scan lineitem  [l_orderkey, l_commitdate, l_receiptdate]   ~600,572
      └ Seq Scan orders  [o_orderkey, o_orderpriority]     ~30,000
          Filters: o_orderdate >= '1993-07-01', o_orderdate < '1993-10-01'
```

> The first child of the hash join probes and the second child is the build side. In a RIGHT_SEMI join the build side is the one kept: an order goes on once any line item finds it in the hash table. EXPLAIN prints the condition as `o_orderkey = o_orderkey`: on the lineitem side, the key is the subquery's copy of the outer `o_orderkey`, which equals `l_orderkey`.

### What each pass changed

1. **deliminator** removed the Delim Join. Joining the subquery to the distinct order keys only to join the result back to orders is the same as joining lineitem to orders directly, since `l_orderkey = o_orderkey` is a plain equality. So the CTE scan of distinct keys disappears, and the MARK join with a filter on its marker becomes a SEMI join: keep each order with at least one match.
2. **filter_pushdown** moved the date range into the orders scan, and the `l_commitdate < l_receiptdate` condition into a Filter right on top of the lineitem scan. A comparison of two columns can't become a scan filter.
3. **build_side_probe_side** builds the hash table on the filtered orders (~30,000 estimated, 5,552 in fact) and lets the late line items (379,809 of 600,572) probe it, which makes it a RIGHT_SEMI join.
4. **unused_columns** cut orders to 2 of its 9 columns and the lineitem scan to 3 of 16.

### Where duckrs differs from DuckDB 1.5.6

1. **DuckDB 1.5.6 keeps the Delim Join.** Its plan still collects the distinct order keys (~27,171), joins the late line items to them, and then does a RIGHT_SEMI join with orders on `IS NOT DISTINCT FROM`. duckrs's plan has one join where 1.5.6 has three operators.
2. **The lineitem side still carries 16 NULL columns.** `SELECT *` in the subquery asked for all 16 lineitem columns. unused_columns stops the scan from reading them, but a projection still puts out a constant NULL for each of them, next to the key it joins on. DuckDB's projection there has only the key. Constant vectors cost almost nothing to make, so this is untidy rather than slow.

- **0.007 s**: Q4 with the optimizer on, median of 11 warm runs, with a load average of about 12. It returns 5 rows, from 989 orders for 4-NOT SPECIFIED to 1,077 for 5-LOW: 5,093 of the quarter's 5,552 orders had a late line item.
- **0.014 s**: Q4 with `SET disabled_optimizers = 'deliminator'`, the median of 11 runs that ranged from 0.008 s to 0.038 s. With the Delim Join kept, the plan builds the distinct key list, joins lineitem to it, and then MARK-joins orders.
- **0.010 s**: Q4 with `SET disabled_optimizers = 'join_filter_pushdown'` (0.008–0.014 s). With this load the overlap with the optimized runs is too large to call it a clear gain.

## TPC-H Q5, with the optimizer off and on

Q5 joins six tables to find the revenue of each Asian nation in 1994. These are real `EXPLAIN` plans from a duckrs release build of the Stage 3 branch, run on TPC-H scale factor 0.1 data (lineitem has 600,572 rows). The plans are redrawn as text trees from the CLI's boxes. Row counts with `~` are the optimizer's estimates, not counted rows.

```
SELECT n_name, sum(l_extendedprice * (1 - l_discount)) AS revenue
FROM customer, orders, lineitem, supplier, nation, region
WHERE c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND l_suppkey = s_suppkey
  AND c_nationkey = s_nationkey
  AND s_nationkey = n_nationkey
  AND n_regionkey = r_regionkey
  AND r_name = 'ASIA'
  AND o_orderdate >= CAST('1994-01-01' AS date)
  AND o_orderdate <  CAST('1995-01-01' AS date)
GROUP BY n_name
ORDER BY revenue DESC;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By revenue DESC                                       ~25
└ Hash Group By n_name, sum(...)                            ~25
  └ Filter (the whole WHERE clause, 9 conditions)
    └ Cross Product                       ~18,446,744,073,709,551,615
      ├ Cross Product                     ~18,446,744,073,709,551,615
      │ ├ Cross Product                   ~1,351,287,000,000,000,000
      │ │ ├ Cross Product                 ~1,351,287,000,000,000
      │ │ │ ├ Cross Product               ~2,250,000,000
      │ │ │ │ ├ Seq Scan customer  (all 8 columns)          ~15,000
      │ │ │ │ └ Seq Scan orders    (all 9 columns)          ~150,000
      │ │ │ └ Seq Scan lineitem    (all 16 columns)         ~600,572
      │ │ └ Seq Scan supplier      (all 7 columns)          ~1,000
      │ └ Seq Scan nation          (all 4 columns)          ~25
      └ Seq Scan region            (all 3 columns)          ~5
```

**Optimizer on: default settings**

```
Projection __internal_decompress_string(n_name), revenue
└ Order By revenue DESC                                     ~5
  └ Projection __internal_compress_string_uhugeint(n_name), revenue
    └ Hash Group By n_name, sum(l_extendedprice * (1.00 - l_discount))   ~5
      └ Projection (n_name compressed)                      ~10,052
        └ Hash Join  n_nationkey = s_nationkey, l_suppkey = s_suppkey   ~10,052
          ├ Hash Join  l_orderkey = o_orderkey                ~12,384
          │ ├ Seq Scan lineitem  [l_orderkey, l_suppkey, l_extendedprice, l_discount]   ~600,572
          │ └ Hash Join  o_custkey = c_custkey              ~6,165
          │   ├ Seq Scan orders  [o_orderkey, o_custkey]    ~30,000
          │   │   Filters: o_orderdate >= '1994-01-01' AND o_orderdate < '1995-01-01'
          │   └ Hash Join  c_nationkey = n_nationkey       ~2,884
          │     ├ Seq Scan customer  [c_custkey, c_nationkey]   ~15,000
          │     │   Filters: c_custkey <= 14999
          │     └ Hash Join  n_regionkey = r_regionkey     ~5
          │       ├ Seq Scan nation  [n_nationkey, n_name, n_regionkey]   ~25
          │       └ Seq Scan region  [r_regionkey]               ~1
          │           Filters: r_name = 'ASIA'
          └ Seq Scan supplier  [s_suppkey, s_nationkey]         ~1,000
```

> In each hash join, the first child probes and the second child (├ then └) is the build side that goes into the hash table.

### What each pass changed

1. **filter_pushdown** split the 9-condition WHERE clause. `r_name = 'ASIA'` went to the region scan and the date range went to the orders scan. Each of the six join conditions went to the pair of tables it reads, which turned all five cross products into hash joins. Without this, the plan would build a cross product estimated at about 1.7 × 10^20 rows. That's more than a 64-bit counter can hold, which is why the estimate shown stops at 18,446,744,073,709,551,615.
2. **filter_combiner** (inside filter pushdown) put `c_nationkey`, `s_nationkey` and `n_nationkey` into one set of equal columns. That's why the plan joins on `c_nationkey = n_nationkey` and `n_nationkey = s_nationkey`, two conditions the query never wrote. It gives the join orderer more ways to connect the tables.
3. **join_order** started from the most selective table and worked outwards. Region filtered to 1 row joins nation (~5). Then come customer (~2,884), orders for 1994 (~6,165), lineitem (~12,384) and, last, supplier on two keys (~10,052). Every intermediate result stays in the thousands.
4. **build_side** made the smaller input of each join the build side. lineitem, the largest table, is never put into a hash table. It only streams through as the probe side.
5. **statistics_propagation** added `c_custkey <= 14999` to the customer scan. The largest `o_custkey` in orders is 14,999, so no customer with a higher key can match. The bound disappears from the plan with `SET disabled_optimizers = 'statistics_propagation'`.
6. **unused_columns** cut each scan down to the columns the query reads: 4 of lineitem's 16 columns, 2 of orders' 9, 2 of customer's 8 and 2 of supplier's 7. The joins copy rows that are that much narrower.
7. **compressed_materialization** packs the `n_name` strings into a fixed-width integer (`uhugeint`) before the GROUP BY and the ORDER BY, and unpacks them in the top projection. Grouping and sorting then compare integers, not strings.

- **0.011 s**: Q5 with the optimizer on, second run. It returns 5 rows, CHINA first. The first run took 2.5 s, which includes working out the table statistics the first time a plan reads them after the load.
- **Not run**: The plan with the optimizer off. It would have to produce the full cross product of the six tables before filtering it.
- **2 / 2**: Row groups of orders scanned. With this little data the date filter can't skip a row group, so the gain here comes from joins and columns, not zone maps.

> To reproduce: load TPC-H SF0.1 (generated with DuckDB's `CALL dbgen(sf=0.1)` and loaded through `read_csv`), then run `EXPLAIN` on `extension/tpch/dbgen/queries/q05.sql`, once as it is and once after `PRAGMA disable_optimizer`.

## TPC-H Q6, with the optimizer off and on

Q6 is the smallest TPC-H query: one table, no GROUP BY and a single sum. All of its work is the filter, so it shows what the optimizer does to a WHERE clause on its own. The plans are from a release build of `phase-55-optimizer-rewriters` at `3bdaec9`, which includes both merged fixes, on the same SF0.1 data.

```
SELECT sum(l_extendedprice * l_discount) AS revenue
FROM lineitem
WHERE l_shipdate >= CAST('1994-01-01' AS date)
  AND l_shipdate <  CAST('1995-01-01' AS date)
  AND l_discount BETWEEN 0.05 AND 0.07
  AND l_quantity < 24;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Ungrouped Aggregate sum(l_extendedprice * l_discount)       ~1
└ Filter  (l_shipdate >= CAST('1994-01-01' AS DATE))
          AND (l_shipdate < CAST('1995-01-01' AS DATE))
          AND (l_discount >= CAST(0.05 AS DECIMAL(15,2)))
          AND (l_discount <= CAST(0.07 AS DECIMAL(15,2)))
          AND (l_quantity < CAST(24 AS DECIMAL(15,2)))   ~120,114
  └ Seq Scan lineitem  (all 16 columns)            ~600,572
```

**Optimizer on: default settings**

```
Ungrouped Aggregate sum(l_extendedprice * l_discount)       ~1
└ Seq Scan lineitem  [l_extendedprice, l_discount]     ~120,114
    Filters: l_shipdate >= '1994-01-01',
             l_shipdate < '1995-01-01',
             (l_discount >= 0.05) AND (l_discount <= 0.07),
             l_quantity < 24.00
```

> The binder already turns `BETWEEN` into two comparisons, so both plans show `>=` and `<=`.

### What each pass changed

1. **filter_pushdown** removed the Filter operator and made the five comparisons scan filters, one per column. The two discount bounds stay together as one filter on `l_discount`. The scan checks them column by column and only passes on the rows that match.
2. **expression_rewriter** folds the constant casts (`CAST('1994-01-01' AS DATE)` becomes the date itself) and splits the AND into separate conditions. Scan filters need plain constants, so the pushdown folds them as well when it moves them into the scan.
3. **unused_columns** cut the scan's output from 16 columns to the 2 the sum reads. `l_shipdate` and `l_quantity` are read for the filters only, and the filter part of `l_discount` reuses the column the sum needs.
4. **statistics_propagation** changed nothing here. Each condition cuts inside its column's range (ship dates 1992-01-03 to 1998-12-01, discounts 0.00 to 0.10, quantities 1 to 50), so none of them is always true or always false. lineitem is stored in order key order, so each of its 5 row groups holds ship dates from early 1992 to late 1998 and zone maps can't skip any.

- **0.002 s**: Q6 with the optimizer on, median of 7 warm runs. It returns 11,803,420.2534. With it off, the time is 0.006 s, about 3.5 times the CPU time (0.043 s of user time against 0.012 s).
- **1.9%**: Rows that pass all four conditions: 11,618 of 600,572. The estimate stays at ~120,114 (20%), the default guess for one comparison: duckrs doesn't combine the selectivity of several filters on one table.
- **Either one**: Of filter_pushdown and expression_rewriter is enough for the faster time. With only one of them switched off, Q6 still takes 0.002–0.003 s. Only with both off does it slow to 0.006 s, the same as with the whole optimizer off: then the Filter operator evaluates the five casts again for every chunk. Switching off unused_columns, with or without filter_pushdown, makes no measurable difference at this size.

> The first run after the load took 0.088 s every time, as in Q5: it includes working out the table statistics the first time a plan reads them. The timings above leave it out.

## TPC-H Q7, with the optimizer off and on

Q7 sums the trade between two nations, France and Germany, in each direction, for 1995 and 1996. It joins six tables, nation twice, and its key condition is an OR that spans both copies of nation. That shows how the optimizer gets a filter for each table out of a condition that reads two of them. The plans are from a release build of `phase-55-optimizer-rewriters` at `1ed6cc1`, on the same SF0.1 data.

```
SELECT supp_nation, cust_nation, l_year, sum(volume) AS revenue
FROM (SELECT n1.n_name AS supp_nation, n2.n_name AS cust_nation,
             extract(year FROM l_shipdate) AS l_year,
             l_extendedprice * (1 - l_discount) AS volume
      FROM supplier, lineitem, orders, customer, nation n1, nation n2
      WHERE s_suppkey = l_suppkey
        AND o_orderkey = l_orderkey
        AND c_custkey = o_custkey
        AND s_nationkey = n1.n_nationkey
        AND c_nationkey = n2.n_nationkey
        AND ((n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY')
          OR (n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE'))
        AND l_shipdate BETWEEN CAST('1995-01-01' AS date)
                           AND CAST('1996-12-31' AS date)) AS shipping
GROUP BY supp_nation, cust_nation, l_year
ORDER BY supp_nation, cust_nation, l_year;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By supp_nation, cust_nation, l_year                   ~9,183
└ Hash Group By supp_nation, cust_nation, l_year, sum(volume)   ~9,183
  └ Projection ...                       ~18,446,744,073,709,551,615
    └ Filter (7 conditions)                ~18,446,744,073,709,551,615
      └ Cross Product × 5  supplier × lineitem × orders × customer × nation × nation
          (all columns of every table)
```

**Optimizer on: default settings**

```
Projection __internal_decompress_string(supp_nation), (cust_nation), l_year, revenue   ~1
└ Order By supp_nation, cust_nation, l_year
  └ Projection __internal_compress_string_uhugeint(...) (both nations)
    └ Hash Group By supp_nation, cust_nation, year(l_shipdate), sum(...)   ~1
      └ Projection ..., __internal_compress_string_uhugeint(n1.n_name), (n2.n_name)   ~15
        └ Hash Join  o_custkey = c_custkey               ~15
              AND (n1 IN (FRANCE, GERMANY)) AND (n2 IN (GERMANY, FRANCE))
              AND ((n1 = FRANCE AND n2 = GERMANY) OR (n1 = GERMANY AND n2 = FRANCE))
          ├ Hash Join  o_orderkey = l_orderkey            ~1,881
          │ ├ Seq Scan orders  [o_orderkey, o_custkey]       ~150,000
          │ └ Hash Join  l_suppkey = s_suppkey            ~3,749
          │   ├ Seq Scan lineitem  [l_orderkey, l_suppkey, l_extendedprice, l_discount, l_shipdate]
          │   │   Filters: l_shipdate >= '1995-01-01' AND l_shipdate <= '1996-12-31'   ~120,114
          │   └ Hash Join  s_nationkey = n1.n_nationkey     ~38
          │     ├ Seq Scan supplier  [s_suppkey, s_nationkey]   ~1,000
          │     └ Filter  n_name = 'FRANCE' OR n_name = 'GERMANY'
          │       └ Filter  (same OR), (same OR)
          │         └ Seq Scan nation n1  Filters: optional: n_name IN ('FRANCE', 'GERMANY') (twice)
          └ Hash Join  c_nationkey = n2.n_nationkey       ~2,884
            ├ Seq Scan customer  [c_custkey, c_nationkey]   Filters: c_custkey <= 14999
            └ Filter  n_name = 'GERMANY' OR n_name = 'FRANCE'
              └ Seq Scan nation n2  Filters: optional: n_name IN ('GERMANY', 'FRANCE')
```

> The first child of each hash join probes and the second child is the build side. The repeated n1 filters, the extra IN conditions in the last join and its ~15 row estimate are where duckrs differs from DuckDB 1.5.6, described below.

### What each pass changed

1. **filter_pushdown** turned the five cross products into hash joins on the five key conditions and moved the ship date range into the lineitem scan.
2. **Filters for each nation from the OR.** The condition `(n1 = FRANCE AND n2 = GERMANY) OR (n1 = GERMANY AND n2 = FRANCE)` reads both copies of nation, so it can only be checked once both are joined. But each branch of the OR names a nation for n1, so every row that passes has n1 as FRANCE or GERMANY, and the same goes for n2. Filter pushdown adds those two conditions and pushes each one to its own nation scan, where it cuts nation from 25 rows to 2. The full OR still runs above the last join to pair them up correctly.
3. **join_order** starts from the two filtered nations. Suppliers of France and Germany (~38 estimated) are the build side for the 1995–96 line items, and their join (~3,749) is the build side for orders. Customers of the two nations (~2,884) are joined on their own and are the build side of the last join.
4. **statistics_propagation** added `c_custkey <= 14999` to the customer scan, as in Q3, Q5, Q10 and Q18.
5. **compressed_materialization** packs the two nation names into 16-byte integers for the GROUP BY and the ORDER BY.
6. **unused_columns** cut every table down to the columns the joins and the sum need: lineitem to 5 of 16, orders, customer and supplier to 2 each.

### Where duckrs differs from DuckDB 1.5.6

1. **The n1 filter is there three times.** The supplier side of nation has one Filter on another, with the same OR three times, and the optional IN filter twice in its scan. DuckDB has each once. With `SET disabled_optimizers = 'expression_rewriter'` duckrs has it once too, so the copies come from how the rewriter and the pushdown work together. On 25 rows of nation it costs nothing measurable, but the copies also feed the estimates.
2. **The last join keeps the derived conditions.** duckrs checks the two IN conditions again in the last join, next to the full OR. DuckDB checks only the full OR, in a Filter above the join.
3. **The estimates are much too low.** duckrs estimates the last join at ~15 rows and the GROUP BY at ~1. In fact 643 rows reach the GROUP BY, and they make 4 groups. DuckDB estimates ~4,187. The French and German line items are 15,631 rows, where duckrs estimates ~3,749 and DuckDB ~18,749. The repeated nation filters are a likely cause, since each copy that is counted as a separate condition lowers the estimate again. I haven't confirmed that. Here the low estimate doesn't change the plan, since both engines pick the same join order.
4. **l_year isn't packed.** DuckDB packs `year(l_shipdate)` into one byte, as `__internal_compress_integral_utinyint(l_year, 1992)`. duckrs groups on the full integer.

- **4 rows**: The result: France to Germany and Germany to France, for 1995 and 1996. Germany's sales to France in 1995 are the largest, 6,232,818.70.
- **643**: Rows that reach the GROUP BY, out of 1,270 line items whose supplier and customer are both French or German. The full OR drops the half where both are from the same nation.
- **Not measured**: The machine's load average was above 70 during these runs, and Q7's times ranged from 0.018 s to 0.141 s, too noisy to compare the passes.

## TPC-H Q8, with the optimizer off and on

Q8 works out Brazil's share of one part type's sales in the Americas, for 1995 and 1996. It joins eight tables, the most of any walkthrough here, with nation twice. Three small filters (one part type, one region, two years of orders) are spread over different ends of the join graph, so join order matters more here than anywhere else. The plans are from the same build as Q7 (`1ed6cc1`) and the same SF0.1 data.

```
SELECT o_year,
       sum(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / sum(volume) AS mkt_share
FROM (SELECT extract(year FROM o_orderdate) AS o_year,
             l_extendedprice * (1 - l_discount) AS volume,
             n2.n_name AS nation
      FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region
      WHERE p_partkey = l_partkey
        AND s_suppkey = l_suppkey
        AND l_orderkey = o_orderkey
        AND o_custkey = c_custkey
        AND c_nationkey = n1.n_nationkey
        AND n1.n_regionkey = r_regionkey
        AND r_name = 'AMERICA'
        AND s_nationkey = n2.n_nationkey
        AND o_orderdate BETWEEN CAST('1995-01-01' AS date) AND CAST('1996-12-31' AS date)
        AND p_type = 'ECONOMY ANODIZED STEEL') AS all_nations
GROUP BY o_year
ORDER BY o_year;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By o_year                                             ~2,811
└ Hash Group By o_year, sum(CASE ...), sum(volume)          ~2,811
  └ Projection ...                         ~18,446,744,073,709,551,615
    └ Filter (10 conditions)                 ~18,446,744,073,709,551,615
      └ Cross Product × 7  part × supplier × lineitem × orders × customer
                           × nation × nation × region   (all columns of every table)
```

**Optimizer on: default settings**

```
Order By o_year                                             ~2
└ Projection o_year, CAST(sum AS DOUBLE) / CAST(sum AS DOUBLE)
  └ Perfect Hash Group By  year(o_orderdate)             ~2
      sum(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END), sum(volume)
    └ Hash Join  s_nationkey = n2.n_nationkey             ~70
      ├ Hash Join  s_suppkey = l_suppkey                  ~73
      │ ├ Seq Scan supplier  [s_suppkey, s_nationkey]     ~1,000
      │ └ Hash Join  c_nationkey = n1.n_nationkey         ~90
      │   ├ Hash Join  c_custkey = o_custkey              ~472
      │   │ ├ Seq Scan customer  [c_custkey, c_nationkey]  Filters: c_custkey <= 14999
      │   │ └ Hash Join  o_orderkey = l_orderkey          ~442
      │   │   ├ Seq Scan orders  [o_orderkey, o_custkey, o_orderdate]   ~30,000
      │   │   │   Filters: o_orderdate BETWEEN 1995-01-01 AND 1996-12-31
      │   │   └ Hash Join  l_partkey = p_partkey          ~4,409
      │   │     ├ Seq Scan lineitem  [l_orderkey, l_partkey, l_suppkey, l_extendedprice, l_discount]   ~600,572
      │   │     └ Seq Scan part  [p_partkey]  Filters: p_type = 'ECONOMY ANODIZED STEEL'   ~137
      │   └ Hash Join  n1.n_regionkey = r_regionkey       ~5
      │     ├ Seq Scan nation n1  [n_nationkey, n_regionkey]   ~25
      │     └ Seq Scan region  [r_regionkey]  Filters: r_name = 'AMERICA'
      └ Seq Scan nation n2  [n_nationkey, n_name]          ~25
```

> The first child of each hash join probes and the second child is the build side.

### What each pass changed

1. **filter_pushdown** turned the seven cross products into hash joins on the seven key conditions and put each one-table filter in its scan: the part type, the region name and the order date range.
2. **join_order** starts where the filters are. The 137 parts of one type (147 in fact) build the first hash table, which all of lineitem probes, so only 4,485 line items go on. Orders of 1995–96 build the next table, and that leaves 1,429. They then meet customers, and those customers are joined to the American nations (n1 with region, 5 nations) in a small join of their own, which leaves 282 rows. Supplier and its nation (n2), which filter nothing, come last. DuckDB 1.5.6 picks exactly the same order.
3. **statistics_propagation** did two things. It added `c_custkey <= 14999` to the customer scan, as in the other queries with customer. And from the order date range it knows `year(o_orderdate)` is 1995 or 1996, so the GROUP BY becomes a **Perfect Hash Group By** over that two-value range, as in DuckDB.
4. **unused_columns** cut part to 1 of its 9 columns, region to 1 of 3, and lineitem to 5 of 16. The filter columns `p_type`, `r_name` and `o_orderdate` are read in their scans; `o_orderdate` also goes up for the year.
5. **join_filter_pushdown** doesn't show in EXPLAIN. At run time the part join hands the lineitem scan the range and a Bloom filter of the 147 part keys, so line items of other parts can be dropped in the scan.

- **0.012 s**: Q8 with the optimizer on, median of 11 warm runs. Brazil's share was 2.86% in 1995 and 1.83% in 1996.
- **0.116 s**: Q8 with `SET disabled_optimizers = 'join_order'`, about 10 times slower in all 11 runs. Without it the joins follow the FROM clause: part and supplier make a cross product of 137,000 rows, lineitem probes it on two keys, and region, the most selective filter, is joined last.
- **0.014 s**: Q8 with `SET disabled_optimizers = 'join_filter_pushdown'`, slower in all 11 runs (0.013–0.015 s against 0.011–0.012 s). The filter of part keys lets the lineitem scan drop rows before they reach the join.

> The estimates are about half of DuckDB's at every join above part (e.g. ~442 against ~828 for orders, ~70 against ~143 at the top). Both are below the actual 282 rows at the top, and the plan is the same.

## TPC-H Q9, with the optimizer off and on

Q9 works out the profit on "green" parts per nation and year. It also joins six tables, but its filter is a `LIKE` on part names, and partsupp joins lineitem on two keys at once. The setup is the same as for Q5: the duckrs Stage 3 release build and TPC-H SF0.1 data.

```
SELECT nation, o_year, sum(amount) AS sum_profit
FROM (
  SELECT n_name AS nation,
         extract(year FROM o_orderdate) AS o_year,
         l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity AS amount
  FROM part, supplier, lineitem, partsupp, orders, nation
  WHERE s_suppkey = l_suppkey
    AND ps_suppkey = l_suppkey
    AND ps_partkey = l_partkey
    AND p_partkey = l_partkey
    AND o_orderkey = l_orderkey
    AND s_nationkey = n_nationkey
    AND p_name LIKE '%green%'
) AS profit
GROUP BY nation, o_year
ORDER BY nation, o_year DESC;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By nation ASC, o_year DESC                            ~66,761
└ Hash Group By nation, o_year, sum(amount)                 ~66,761
  └ Projection nation, date_part('YEAR', o_orderdate), amount
    └ Filter (7 conditions, including p_name ~~ '%green%')
      └ Cross Product                       ~18,446,744,073,709,551,615
        ├ Cross Product                     ~18,446,744,073,709,551,615
        │ ├ Cross Product                   ~960,915,200,000,000,000
        │ │ ├ Cross Product                 ~12,011,440,000,000
        │ │ │ ├ Cross Product               ~20,000,000
        │ │ │ │ ├ Seq Scan part      (all 9 columns)          ~20,000
        │ │ │ │ └ Seq Scan supplier  (all 7 columns)          ~1,000
        │ │ │ └ Seq Scan lineitem    (all 16 columns)         ~600,572
        │ │ └ Seq Scan partsupp      (all 5 columns)          ~80,000
        │ └ Seq Scan orders          (all 9 columns)          ~150,000
        └ Seq Scan nation            (all 4 columns)          ~25
```

**Optimizer on: default settings**

```
Projection __internal_decompress_string(nation), o_year, sum_profit
└ Order By nation ASC, o_year DESC                          ~25
  └ Projection __internal_compress_string_uhugeint(nation), o_year, sum_profit
    └ Projection nation, o_year, sum_profit
      └ Hash Group By nation, year(o_orderdate), sum(...)   ~25
        └ Projection (n_name compressed)                    ~27,371
          └ Hash Join  o_orderkey = l_orderkey              ~27,371
            ├ Seq Scan orders  [o_orderkey, o_orderdate]      ~150,000
            └ Hash Join  l_suppkey = ps_suppkey, l_partkey = ps_partkey   ~54,561
              ├ Seq Scan lineitem  [l_orderkey, l_partkey, l_suppkey,
              │                     l_quantity, l_extendedprice, l_discount]   ~600,572
              └ Hash Join  ps_suppkey = s_suppkey            ~7,267
                ├ Hash Join  ps_partkey = p_partkey          ~9,365
                │ ├ Seq Scan partsupp  [ps_partkey, ps_suppkey, ps_supplycost]   ~80,000
                │ └ Seq Scan part  [p_partkey]                ~4,000
                │     Filters: contains(p_name, 'green')
                └ Hash Join  s_nationkey = n_nationkey        ~961
                  ├ Seq Scan supplier  [s_suppkey, s_nationkey]   ~1,000
                  └ Seq Scan nation  [n_nationkey, n_name]        ~25
```

> As before, the first child of each hash join probes and the second child is the build side.

### What each pass changed

1. **The LIKE rule of the expression rewriter** (`like_optimizations` in `rewriter.rs`) turned `p_name LIKE '%green%'` into `contains(p_name, 'green')`. A pattern with wildcards only at its ends is a plain substring search, which is cheaper than matching a LIKE pattern.
2. **filter_pushdown** sent the `contains` filter into the part scan and turned the five cross products into hash joins. With the optimizer off, the cross product reaches about 3.6 × 10^27 rows before the filter runs. As in Q5, the estimate stops at the 64-bit limit.
3. **filter_combiner** put `s_suppkey`, `l_suppkey` and `ps_suppkey` into one set of equal columns, and did the same for `p_partkey`, `l_partkey` and `ps_partkey`. The query only ever compares lineitem with the other tables. Thanks to these sets, the plan can join partsupp with part (`ps_partkey = p_partkey`) and with supplier (`ps_suppkey = s_suppkey`) before lineitem is touched.
4. **join_order** used that freedom. It joins the small tables first: green parts (~4,000) with partsupp (~9,365), then with supplier and nation (~7,267). The result is a hash table keyed on both `(suppkey, partkey)`. lineitem probes it on both keys at once, so the lineitem rows for parts that aren't green are dropped in that one join (~54,561 left).
5. **build_side** left orders (~150,000 rows) as the probe side of the last join and built on the smaller join result (~54,561). C++ compares rows times row width, and by that measure it would build on all of orders here. duckrs only lets width decide when the widths differ several times over, and `build_side.rs` names Q9 as the reason.
6. **unused_columns** cut each scan down to the columns the query reads: 6 of lineitem's 16, 3 of partsupp's 5, 2 of orders' 9 and 2 of supplier's 7. The part scan outputs only `p_partkey`. Its `p_name` is read inside the scan for the filter and goes no further.
7. **compressed_materialization** packs the nation names into a `uhugeint` for the GROUP BY and the ORDER BY, as in Q5.

- **0.026 s**: Q9 with the optimizer on, second run. It returns 175 rows: 25 nations × 7 years. The first run took 0.028 s, because the EXPLAIN before it had already worked out the table statistics.
- **Not run**: The plan with the optimizer off. Its cross product of six tables is about 3.6 × 10^27 rows.
- **2 keys**: The one join where lineitem meets partsupp, part and supplier. The query never wrote that join; the equal-column sets made it possible.

## TPC-H Q10, with the optimizer off and on

Q10 finds the 20 customers who lost the most revenue to returned items in one quarter. It joins four tables and groups by seven customer columns, so it shows a pass the other walkthroughs don't: **partial_aggregate_pushdown**, which sums the line items before the join that adds the customer columns. The plans are from the same build as Q6 (`3bdaec9`) and the same SF0.1 data.

```
SELECT c_custkey, c_name,
       sum(l_extendedprice * (1 - l_discount)) AS revenue,
       c_acctbal, n_name, c_address, c_phone, c_comment
FROM customer, orders, lineitem, nation
WHERE c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND o_orderdate >= CAST('1993-10-01' AS date)
  AND o_orderdate <  CAST('1994-01-01' AS date)
  AND l_returnflag = 'R'
  AND c_nationkey = n_nationkey
GROUP BY c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment
ORDER BY revenue DESC
LIMIT 20;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Top N 20 BY revenue DESC                                    ~20
└ Projection ...
  └ Hash Group By c_custkey, c_name, ..., c_comment, sum(...)   ~6.8 × 10^15
    └ Filter (6 conditions)                       ~6.8 × 10^15
      └ Cross Product                           ~3.4 × 10^16
        ├ Cross Product                         ~1.35 × 10^15
        │ ├ Cross Product  customer × orders     ~2,250,000,000
        │ └ Seq Scan lineitem  (all 16 columns)       ~600,572
        └ Seq Scan nation  (all 4 columns)            ~25
```

**Optimizer on: default settings**

```
Top N 20 BY revenue DESC                                    ~20
└ Projection ..., finalize(state) AS revenue
  └ Projection CAST(c_custkey AS BIGINT), ..., __internal_decompress_string(c_phone), (n_name)
    └ Hash Group By c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment
                    combine_aggr(state)                ~10,329
      └ Projection CAST(c_custkey AS SMALLINT), __internal_compress_string_uhugeint(c_phone), (n_name), ...
        └ Hash Join  o_custkey = c_custkey              ~10,329
          ├ Hash Group By o_custkey                     ~10,052
          │   sum(l_extendedprice * (1.00 - l_discount))  (exports its state)
          │ └ Hash Join  l_orderkey = o_orderkey          ~20,085
          │   ├ Seq Scan lineitem  [l_orderkey, l_extendedprice, l_discount]   ~200,191
          │   │   Filters: l_returnflag = 'R'
          │   └ Seq Scan orders  [o_orderkey, o_custkey]      ~30,000
          │       Filters: o_orderdate >= '1993-10-01', o_orderdate < '1994-01-01'
          └ Hash Join  c_nationkey = n_nationkey          ~14,423
            ├ Seq Scan customer  [c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_comment]
            │   Filters: c_custkey <= 14999                 ~15,000
            └ Seq Scan nation  [n_nationkey, n_name]        ~25
```

> The first child of each hash join probes and the second child is the build side. The plan is shortened: the optimized EXPLAIN also has projections that only cast or reorder columns.

### What each pass changed

1. **filter_pushdown** sent the return flag into the lineitem scan and the quarter into the orders scan, and turned the three cross products into hash joins on the three join conditions.
2. **join_order** joins lineitem with the filtered orders first (orders builds). customer is joined to nation on its own (nation builds, 25 rows), and that result is the build side for the last join. DuckDB 1.5.6 orders it differently: lineitem probes a join of orders with customer and nation.
3. **partial_aggregate_pushdown** saw that the sum reads lineitem only and that `c_custkey = o_custkey` fixes the customer. So it groups the lineitem ⋈ orders rows by `o_custkey` first and exports the sum's state. The GROUP BY above the customer join only combines those states (`combine_aggr`), and `finalize` turns them into the revenue. The customer join then sees one row per customer (3,767) instead of one per line item (11,439). This is a port of C++ `partial_aggregate_pushdown.cpp` from the baseline source, which is newer than DuckDB 1.5.6. 1.5.6 doesn't push the aggregate.
4. **statistics_propagation** added `c_custkey <= 14999` to the customer scan, as in Q3, Q5 and Q18.
5. **compressed_materialization** narrows three of the seven grouping keys: `c_custkey` to a SMALLINT (it is at most 14,999), and the short strings `c_phone` and `n_name` to 16-byte integers. They're unpacked after the GROUP BY. `c_name`, `c_address` and `c_comment` are too long to pack.
6. **unused_columns** cut lineitem to 3 of its 16 columns and orders to 2 of 9. customer keeps 7 of its 8 columns, since the query returns most of them.
7. **top_n** keeps 20 of the 3,767 customers in a heap instead of sorting them all.

- **0.045 s**: Q10 with the optimizer on, median of 15 warm runs. Customer 8242 (ETHIOPIA) comes first, with 622,786.73 of lost revenue.
- **11,439 → 3,767**: Line items that are returned and in the quarter (out of 148,301 returned and 5,677 orders), and the customers they belong to. The estimates were ~20,085 and ~10,052.
- **No clear gain**: Switching off partial_aggregate_pushdown, compressed_materialization or both gave medians of 0.037–0.042 s. The machine's load average was 24–30, and CPU time went the other way, so these differences are noise. At SF0.1 the customer join is too small for pushing the aggregate below it to show: it saves a join over about 7,700 rows and adds a second GROUP BY.

## TPC-H Q12, with the optimizer off and on

Q12 counts the late line items shipped by mail or ship in 1994, split by order priority. Its WHERE clause chains three dates (`l_shipdate < l_commitdate < l_receiptdate`) and has a short `IN` list, which shows two things the other walkthroughs don't: filters derived from other filters, and filters a scan can only use partly. The plans are from a release build of `phase-55-optimizer-rewriters` at `1ed6cc1`, on the same SF0.1 data.

```
SELECT l_shipmode,
       sum(CASE WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH'
                THEN 1 ELSE 0 END) AS high_line_count,
       sum(CASE WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH'
                THEN 1 ELSE 0 END) AS low_line_count
FROM orders, lineitem
WHERE o_orderkey = l_orderkey
  AND l_shipmode IN ('MAIL', 'SHIP')
  AND l_commitdate < l_receiptdate
  AND l_shipdate < l_commitdate
  AND l_receiptdate >= CAST('1994-01-01' AS date)
  AND l_receiptdate <  CAST('1995-01-01' AS date)
GROUP BY l_shipmode
ORDER BY l_shipmode;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Order By l_shipmode ASC                                     ~2
└ Hash Group By l_shipmode, sum(CASE ...), sum(CASE ...)    ~2
  └ Filter (6 conditions)                         ~18,017,160,000
    └ Cross Product                             ~90,085,800,000
      ├ Seq Scan orders    (all 9 columns)             ~150,000
      └ Seq Scan lineitem  (all 16 columns)            ~600,572
```

**Optimizer on: default settings**

```
Projection __internal_decompress_string(l_shipmode), high_line_count, low_line_count   ~2
└ Order By l_shipmode ASC                                   ~2
  └ Projection __internal_compress_string_ubigint(l_shipmode), ...
    └ Hash Group By l_shipmode, sum(CASE ...), sum(CASE ...)   ~2
      └ Projection o_orderkey, o_orderpriority, l_orderkey, __internal_compress_string_ubigint(l_shipmode)
        └ Hash Join  o_orderkey = l_orderkey               ~12,050
          ├ Seq Scan orders  [o_orderkey, o_orderpriority]   ~150,000
          └ Filter  l_commitdate < l_receiptdate,
                    l_shipdate < l_commitdate,
                    (l_shipmode = 'MAIL') OR (l_shipmode = 'SHIP')
            └ Seq Scan lineitem  [l_orderkey, l_shipdate, l_commitdate, l_receiptdate, l_shipmode]   ~120,114
                Filters: l_receiptdate >= '1994-01-01', l_receiptdate < '1995-01-01',
                         l_commitdate < '1995-01-01', l_shipdate < '1995-01-01',
                         optional: l_shipmode IN ('MAIL', 'SHIP')
```

> The first child of the hash join probes and the second child is the build side.

### What each pass changed

1. **filter_pushdown** turned the cross product into a hash join on the order key and moved the lineitem conditions below it. Its FilterCombiner also **derived two filters** the query doesn't state. From `l_commitdate < l_receiptdate` and `l_receiptdate < '1995-01-01'` it follows that `l_commitdate < '1995-01-01'`, and from that, with `l_shipdate < l_commitdate`, that `l_shipdate < '1995-01-01'`. A filter on one column against a constant can go into the scan, where zone maps can use it. The comparisons between two columns can't, so they stay in a Filter above the scan.
2. **The IN list is used twice.** The scan gets `l_shipmode IN ('MAIL', 'SHIP')` as an optional filter: zone maps may use it to skip row groups, but it doesn't remove rows. The Filter above keeps the real check. The **in_clause** pass only turns long IN lists into joins, and it spells a short one out as ORs, as C++ does. With `SET disabled_optimizers = 'in_clause'` the Filter shows the IN instead.
3. **join_order** builds the hash table on the filtered lineitem side (~12,050 rows estimated, 3,155 in fact) and probes it with all 150,000 orders, which have no filter of their own.
4. **join_filter_pushdown** doesn't show in EXPLAIN. At run time the join gives the orders scan the range and a Bloom filter of the 3,155 order keys it built, so orders whose key isn't among them can be dropped in the scan before they reach the join.
5. **compressed_materialization** packs `l_shipmode` (at most 7 characters) into an 8-byte integer (`ubigint`) for the GROUP BY and the ORDER BY, and unpacks it at the top.
6. **unused_columns** cut orders to 2 of its 9 columns and lineitem to 5 of 16.

- **0.007 s**: Q12 with the optimizer on, median of 11 warm runs, with a load average of about 3. It returns two rows: MAIL with 647 high-priority and 945 low-priority line items, SHIP with 620 and 943.
- **0.011 s**: Q12 with `SET disabled_optimizers = 'join_filter_pushdown'`, slower in all 11 runs (0.010–0.011 s against 0.007–0.008 s). Here the runtime filter pays: it drops orders whose key isn't among the 3,155 the build side holds.
- **0.006 s**: Q12 with `SET disabled_optimizers = 'filter_pushdown'`, slightly faster in all 11 runs. The join is still made (join_order finds the condition), but no filter goes into the scan and none is derived. The two derived filters remove only 2,453 of the 92,050 rows received in 1994, and the IN can't skip a row group, since every row group holds every ship mode. So checking them in the scan costs a little more than it saves.

> DuckDB 1.5.6 makes the same plan: the same derived filters, the optional IN filter in the scan, and lineitem as the build side.

## TPC-H Q14, with the optimizer off and on

Q14 works out what share of one month's revenue came from promotional parts. It is Q6 with one join added: lineitem filtered to September 1995, joined to part, and a `LIKE` inside the aggregate. The plans are from the same build as Q6 (`3bdaec9`) and the same SF0.1 data.

```
SELECT 100.00 * sum(CASE WHEN p_type LIKE 'PROMO%'
                         THEN l_extendedprice * (1 - l_discount)
                         ELSE 0 END)
       / sum(l_extendedprice * (1 - l_discount)) AS promo_revenue
FROM lineitem, part
WHERE l_partkey = p_partkey
  AND l_shipdate >= date '1995-09-01'
  AND l_shipdate < CAST('1995-10-01' AS date);
```

**Optimizer off: PRAGMA disable_optimizer**

```
Projection CAST(100.00 * sum AS DOUBLE) / CAST(sum AS DOUBLE)   ~1
└ Ungrouped Aggregate
    sum(CASE WHEN p_type ~~ 'PROMO%' THEN ... ELSE CAST(0 AS DECIMAL(18,4)) END),
    sum(l_extendedprice * (CAST(1 AS DECIMAL(16,2)) - l_discount))
  └ Filter  l_partkey = p_partkey AND l_shipdate (2 conditions)   ~2,402,288,000
    └ Cross Product                                   ~12,011,440,000
      ├ Seq Scan lineitem  (all 16 columns)             ~600,572
      └ Seq Scan part      (all 9 columns)              ~20,000
```

**Optimizer on: default settings**

```
Projection CAST(100.00 * sum AS DOUBLE) / CAST(sum AS DOUBLE)   ~1
└ Ungrouped Aggregate
    sum(CASE WHEN prefix(p_type, 'PROMO') THEN ... ELSE 0.0000 END),
    sum(l_extendedprice * (1.00 - l_discount))
  └ Hash Join  l_partkey = p_partkey                 ~128,746
    ├ Seq Scan lineitem  [l_partkey, l_extendedprice, l_discount]   ~120,114
    │   Filters: l_shipdate >= '1995-09-01', l_shipdate < '1995-10-01'
    └ Seq Scan part  [p_partkey, p_type]              ~20,000
```

> The first child of the hash join probes and the second child is the build side.

### What each pass changed

1. **filter_pushdown** turned the cross product into a hash join on `l_partkey = p_partkey` and moved the ship date range into the lineitem scan, as in Q6. Without it the plan pairs every one of 600,572 line items with every one of 20,000 parts, about 1.2 × 10^10 rows, before filtering.
2. **join_order** and **build_side_probe_side** build the hash table on part, the side estimated smaller (20,000 rows against ~120,114). Switching off `build_side_probe_side` gives the same plan: join_order had already put part on the build side.
3. **expression_rewriter** turned `p_type LIKE 'PROMO%'` into `prefix(p_type, 'PROMO')`, which compares the first five bytes and doesn't run the general LIKE matcher. It also folded the constant casts (`CAST(1 AS DECIMAL(16,2))` becomes `1.00`).
4. **unused_columns** cut lineitem to 3 of its 16 columns and part to 2 of its 9. `l_shipdate` is read inside the scan for its filter only.
5. **join_filter_pushdown** doesn't show in EXPLAIN. At run time the join hands the lineitem scan a range and a Bloom filter of the part keys it built. Here the build side holds every part key, so the filter can't drop any line item.

- **0.009 s**: Q14 with the optimizer on, median of 9 warm runs, with a load average of about 13 on the machine. It returns 16.28: 16.28% of September 1995's revenue came from promotional parts.
- **7,630**: Line items shipped in September 1995, against an estimate of ~120,114. All 7,630 find their part, and 3,309 of the 20,000 parts are PROMO types.
- **0.007 s**: Q14 with `SET disabled_optimizers = 'join_filter_pushdown'`, faster in all 9 runs. Since the filter drops nothing here, checking it is pure cost. Switching off `expression_rewriter` (0.010 s) or `build_side_probe_side` (0.009 s) makes no clear difference: the `LIKE` only runs on the 7,630 joined rows.

> DuckDB 1.5.6 makes the same plan: a hash join that builds on part, the same three lineitem columns and `prefix(p_type, 'PROMO')`. It estimates the join at ~120,114 rows where duckrs says ~128,746, and puts the CASE in a projection under the aggregate where duckrs evaluates it inside the aggregate.

## TPC-H Q18, with the optimizer off and on

Q18 finds the 100 largest orders whose line items add up to a quantity over 300. Unlike Q5 and Q9, it has an `IN` subquery, and both the subquery and the outer query compute `sum(l_quantity)` per order. The setup is the same: the duckrs Stage 3 release build and TPC-H SF0.1 data.

```
SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity)
FROM customer, orders, lineitem
WHERE o_orderkey IN (
        SELECT l_orderkey
        FROM lineitem
        GROUP BY l_orderkey
        HAVING sum(l_quantity) > 300)
  AND c_custkey = o_custkey
  AND o_orderkey = l_orderkey
GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice
ORDER BY o_totalprice DESC, o_orderdate
LIMIT 100;
```

**Optimizer off: PRAGMA disable_optimizer**

```
Top N 100 BY o_totalprice DESC, o_orderdate ASC             ~100
└ Hash Group By c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice,
                sum(l_quantity)                             ~270,257,393,772,649
  └ Filter  #mark AND c_custkey = o_custkey AND o_orderkey = l_orderkey
    └ Hash Join MARK  o_orderkey = l_orderkey (of the subquery)   ~1,351,287,000,000,000
      ├ Cross Product                     ~1,351,287,000,000,000
      │ ├ Cross Product                   ~2,250,000,000
      │ │ ├ Seq Scan customer  (all 8 columns)            ~15,000
      │ │ └ Seq Scan orders    (all 9 columns)            ~150,000
      │ └ Seq Scan lineitem    (all 16 columns)           ~600,572
      └ Projection l_orderkey                             ~59,802
        └ Filter sum > 300                                ~59,802
          └ Hash Group By l_orderkey, sum(l_quantity)     ~299,011
            └ Seq Scan lineitem  (all 16 columns)         ~600,572
```

**Optimizer on: default settings**

```
Top N 100 BY o_totalprice DESC, o_orderdate ASC             ~100
└ Projection c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum
  └ Projection ..., finalize(sum state)
    └ Hash Group By c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice,
                    combine_aggr(sum state)                 ~32,062
      └ Projection CAST(o_orderkey AS INTEGER), ..., CAST(c_custkey AS SMALLINT), ...
        └ Hash Join  o_custkey = c_custkey                ~32,062
          ├ Hash Join INNER  o_orderkey = l_orderkey (brings the sum state along)   ~29,999
          │ ├ Seq Scan orders  [o_orderkey, o_custkey, o_totalprice, o_orderdate]   ~150,000
          │ └ Filter finalize(sum) > 300.00                ~59,802
          │   └ Projection CAST(l_orderkey AS BIGINT), sum state   ~299,011
          │     └ Hash Group By l_orderkey, sum(l_quantity)  ~299,011
          │       └ Projection CAST(l_orderkey AS INTEGER), l_quantity
          │         └ Seq Scan lineitem  [l_orderkey, l_quantity]   ~600,572
          └ Seq Scan customer  [c_custkey, c_name]         ~15,000
              Filters: c_custkey <= 14999
```

> The first child of each hash join probes and the second child is the build side. With the optimizer on, lineitem is scanned once. With it off, lineitem is scanned twice.

### What each pass changed

1. **filter_pushdown** turned the two cross products into hash joins. Because the `IN` is only used as a filter, it also turned the MARK join, which marks each row as in or not in the subquery, into a SEMI join that just drops the rows that aren't in it.
2. **join_order** joins orders with the subquery's result first, which keeps only the orders over 300, and adds customer after that. The outer join with lineitem no longer comes before the filter.
3. **aggregate_reuse** made the biggest change. The outer query sums `l_quantity` per order, and the subquery already sums the same column of the same table per order. So the SEMI join becomes an INNER join that carries the subquery's sum state along. The outer GROUP BY then combines those states with `combine_aggr` and reads out the result with `finalize`. The outer join with lineitem and the second lineitem scan are gone. This is the example given in `aggregate_reuse.rs`.
4. **statistics_propagation** added `c_custkey <= 14999` to the customer scan, the same bound as in Q5, from the largest `o_custkey` in orders.
5. **compressed_materialization** narrows the integer keys for the two GROUP BYs: `l_orderkey` and `o_orderkey` go from BIGINT to INTEGER, and `c_custkey` to SMALLINT, because statistics show the values fit. The four casts disappear with `SET disabled_optimizers = 'compressed_materialization'`.
6. **unused_columns** cut lineitem from 16 columns to 2, and orders to 4 of 9.
7. **top_n** is in both plans. It has no switch, so it runs even after `PRAGMA disable_optimizer`.

- **0.090 s**: Q18 with the default settings, median of 5 warm runs. It returns 5 rows: at SF0.1 only 5 orders have a quantity over 300.
- **0.022 s**: Q18 with `SET disabled_optimizers = 'aggregate_reuse'`, median of 5 runs alternated with the ones before. With the reuse off, the plan has a SEMI join and two scans of lineitem.
- **Not run**: The plan with the optimizer off. Its cross product of customer, orders and lineitem is estimated at 1.35 × 10^15 rows.

> **Open finding:** in duckrs today, aggregate reuse saves a scan of lineitem but makes Q18 about 4× slower at this scale. DuckDB 1.5.6 doesn't apply this rewrite to Q18; its plan keeps the SEMI join and both lineitem scans. The pass comes from the newer DuckDB source that duckrs is ported from.

> **Cause:** the plan is right; the cost is in how duckrs writes out exported aggregate states. Grouping lineitem by order takes 0.024 s as a plain `sum`, 0.056 s when it exports its states, and 0.085 s when `finalize` then reads them back. In a profile, 81% of the busy CPU time is in the hash aggregate's output loop (`PhysicalAggregate::set_output`), which handles the 150,000 groups one row at a time. For each row, `write_state` calls `LogicalType::state_layout()`, which copies the whole state type and then frees it. `Vector::set_value` then compares the value's type with the column's type, and that comparison formats the state's constants into strings twice (`StateInfo::constants_key`). Any plan with exported states pays this cost, which also includes partial aggregate pushdown and grouping sets.

> **Fixed, merged into phase-55-optimizer-rewriters (081d0b9):** the output loop now works out the state layout once per column, comparing two types that share their info returns at once, constants are only formatted when there are some, and `finalize()` looks up its aggregate once per vector. Measured against a clean build of the same commit: Q18 with reuse goes from 0.089 s to 0.048 s, and the GROUP BY that exports states from 0.054 s to 0.029 s, about the cost of a plain one. Q18 without reuse stays at 0.020 s, so the reuse plan is still slower. What's left is the row-at-a-time `finalize` of 150,000 states in the HAVING filter.

## Every pass, what it does and why it's faster

Each pass is marked **new** (added in Stage 3), **changed** (existed and was extended) or **existing** (existed already and was not changed). Its switch is the name to use in `disabled_optimizers`.

### Simplify expressions (cheaper work per row)

- **expression_rewriter** (changed; switch: `expression_rewriter`). Constant folding and simplification of AND/OR, CASE, arithmetic, LIKE and comparisons.
  Example: `WHERE x + 0 > 2 * 3   →   WHERE x > 6`
  Faster because: Runs once per row, so a simpler expression saves time on every row.

- **monotone_preimage** (new; switch: `expression_rewriter`). For a monotone function, turns f(col) OP c into a range on col.
  Example: `year(dt) = 2020   →   dt BETWEEN '2020-01-01' AND '2020-12-31'`
  Faster because: Zone maps and indexes can use a range on the column, but not a function of it.

- **or_range_merge** (new; switch: `expression_rewriter`). Merges ranges on the same expression joined by OR when they overlap or touch.
  Example: `x BETWEEN 1 AND 20 OR x BETWEEN 21 AND 40   →   x BETWEEN 1 AND 40`
  Faster because: One range check instead of two, and the result can be pushed into a scan.

- **in_clause_rewriter** (new; switch: `in_clause`). A long IN list becomes a MARK hash join with the constants.
  Example: `WHERE x IN (1, 2, …, 500)`
  Faster because: One hash lookup per row instead of up to 500 comparisons.

- **reorder_filter** (new; switch: `reorder_filter`). Runs the cheapest predicates of a filter first, unless one of them can fail.
  Example: `WHERE int_col = 5 AND regexp_matches(s, '…')`
  Faster because: The expensive regex only runs on the rows the cheap check let through.

### Move filters (drop rows as early as possible)

- **filter_pullup** (new; switch: `filter_pullup`). Lifts filters above inner joins and cross products, so pushdown can send them to every side they apply to.
  Example: `a.i = b.i AND a.i = 5   →   b.i = 5 as well`
  Faster because: Both sides of the join get filtered, not just one.

- **filter_pushdown** (changed; switch: `filter_pushdown`). Moves filters as far down the plan as they go, and turns a cross product with a condition into a join.
  Example: `FROM a, b WHERE a.k = b.k AND a.x > 3`
  Faster because: A filter three joins lower can drop most rows before any hash table is built.

- **filter_combiner** (new; switch: `filter_pushdown`). Groups equal expressions and prunes or derives bounds between them. Runs inside filter pushdown.
  Example: `x > 5 AND x > 3  →  x > 5;   x = 5 AND x > 7  →  no rows`
  Faster because: Removes redundant checks, finds filters that can never hold, and derives new ones.

- **statistics_propagation (filters)** (changed; switch: `statistics_propagation`). Uses min/max statistics to decide a filter before running it. If it always holds it goes away. If it never holds the plan is empty.
  Example: `WHERE age > 500   -- column max is 120`
  Faster because: Rows that can't match are never read.

- **scan_filters** (changed; switch: `filter_pushdown`). Puts filters on one column into the scan itself.
  Example: `WHERE id > 1000000`
  Faster because: The scan can skip whole row groups by their zone maps.

- **cte_filter_pusher** (new; switch: `cte_filter_pusher`). When every read of a materialized CTE is filtered, the OR of those filters goes into the CTE.
  Example: `CTE read with region = 'EU' and with region = 'US'`
  Faster because: The CTE never computes rows that no reader keeps.

- **empty_result_pullup** (existing; switch: `empty_result_pullup`). An operator over an input known to be empty returns nothing.
  Example: `JOIN with a side that statistics prove is empty`
  Faster because: Skips the whole subtree.

### Joins (where most of the speedup is)

- **join_order** (changed; switch: `join_order`). DPhyp dynamic programming (greedy for many tables) picks the cheapest order of inner joins, using C++'s cardinality estimates.
  Example: `TPC-H Q5, Q9: join the small filtered tables first`
  Faster because: A bad order can make intermediate results 1000× larger.

- **distinct_statistics / relation_statistics** (new; switch: `join_order`). HyperLogLog distinct counts and column domains, computed as C++ does, which feed the join cardinality estimates.
  Example: `estimating |orders ⋈ customers|`
  Faster because: Join order is only as good as these estimates.

- **build_side** (changed; switch: `build_side_probe_side`). The hash table is built on the smaller side, weighed by row width. SEMI and ANTI joins can swap sides.
  Example: `small dimension builds, big fact table probes`
  Faster because: A smaller hash table fits in cache and builds faster.

- **join_elimination** (new; switch: `join_elimination`). Removes a LEFT join whose right side nothing reads, when the join key is unique there.
  Example: `SELECT o.* FROM orders o LEFT JOIN (SELECT DISTINCT k FROM t) r ON o.k = r.k`
  Faster because: The join disappears completely.

- **outer_join_simplification** (new; switch: `outer_join_simplification`). A LEFT join becomes INNER when a filter above removes its NULL rows. LEFT JOIN … WHERE r.k IS NULL becomes an ANTI join.
  Example: `LEFT JOIN r … WHERE r.x > 0`
  Faster because: Inner joins can be reordered. Outer joins can't.

- **deliminator** (changed; switch: `deliminator`). Removes the extra joins that flattening a correlated subquery leaves behind.
  Example: `WHERE x > (SELECT avg(y) FROM t2 WHERE t2.k = t1.k)`
  Faster because: Fewer joins and hash tables.

- **disjunctive_join_rewriter** (new; switch: `disjunctive_join_rewriter`). A join on a = x OR b = y becomes one hash join per equality over materialized CTEs.
  Example: `ON t1.a = t2.x OR t1.b = t2.y`
  Faster because: Hash joins instead of a nested loop join that compares every pair of rows.

- **range_joins** (new; switch: no separate switch). Picks a hash, IE, piecewise merge or nested loop join from the comparisons and the row counts.
  Example: `ON t1.a < t2.b AND t1.c > t2.d   →   IE join`
  Faster because: Each kind of condition gets the operator that suits it.

- **projection_pullup** (new; switch: `projection_pullup`). Moves projections that sit between joins above them.
  Example: `a view over joins, joined again`
  Faster because: join_order sees all the joins together and can reorder all of them.

- **unnest_rewriter** (new; switch: `unnest_rewriter`). A LATERAL subquery that only unnests the joined row becomes a plain UNNEST.
  Example: `FROM t, (SELECT UNNEST(t.l))`
  Faster because: The subquery no longer runs once per row.

### Aggregation (group less, or group sooner)

- **aggregate_function_rewriter** (new; switch: `aggregate_function_rewriter`). avg(x) becomes sum/count, which it can share with other aggregates. sum(x + C) becomes sum(x) + C·count(x).
  Example: `SELECT avg(x), sum(x), count(x) FROM t`
  Faster because: Fewer aggregate states to keep up to date.

- **common_aggregate** (new; switch: `common_aggregate`). Keeps each identical aggregate once and points everything that read a copy at it. Runs after statistics, so it also merges the count_star() that the avg rewrite produced.
  Example: `SELECT sum(x), avg(x), count(*) … GROUP BY g`
  Faster because: Fewer aggregate states per group: TPC-H Q1 goes from 11 to 6.

- **partial_aggregate_pushdown** (new; switch: `partial_aggregate_pushdown`). Aggregates one or both sides of a join before the join (eager group-by).
  Example: `SELECT d.name, sum(f.v) FROM fact f JOIN dim d … GROUP BY d.name`
  Faster because: The join sees one row per key instead of millions.

- **aggregate_reuse** (new; switch: `aggregate_reuse`). Reuses aggregate states that a SEMI join or a materialized CTE already computed.
  Example: `WHERE k IN (SELECT k … GROUP BY k HAVING sum(v) > 25), plus sum(v) again`
  Faster because: The table is read and aggregated once.

- **scalar_aggregate_merge** (new; switch: `scalar_aggregate_merge`). Several scalar subqueries on the same table become one scan with FILTER clauses.
  Example: `SELECT (SELECT count(*) FROM t WHERE a < 5), (SELECT sum(b) FROM t WHERE a >= 5)`
  Faster because: One scan of t instead of two.

- **self_join_aggregate** (new; switch: `self_join_aggregate`). A self-join on k with a.x <> b.x becomes GROUP BY k HAVING min(x) <> max(x).
  Example: `TPC-DS q95`
  Faster because: A single scan replaces a join that can blow up quadratically.

- **grouping_sets** (new; switch: `grouping_sets`). ROLLUP and CUBE become one aggregate per set. Coarser sets combine the states of finer ones.
  Example: `GROUP BY ROLLUP(a, b, c)`
  Faster because: The input is aggregated once instead of once per set.

- **perfect hash group-by** (changed; switch: `statistics_propagation`). Groups on small integer ranges that statistics prove use an array index instead of a hash table. Decided again after compressed_materialization, from the ranges of the compressed keys.
  Example: `GROUP BY tinyint_col`
  Faster because: No hashing and no collision handling.

### Sorting, LIMIT and windows (skip sorts that aren't needed)

- **top_n** (existing; switch: no separate switch). ORDER BY … LIMIT k becomes a Top-N. A small LIMIT goes below projections.
  Example: `ORDER BY score DESC LIMIT 10`
  Faster because: Keeps k rows in a heap instead of sorting everything.

- **late_materialization** (new; switch: `late_materialization`). Top-N, LIMIT or SAMPLE over a scan reads only the sort key and the row id, then fetches the other columns for the k rows it picked.
  Example: `SELECT * FROM wide_t ORDER BY a LIMIT 5`
  Faster because: The wide columns are read for 5 rows, not all of them.

- **topn_window_elimination** (new; switch: `top_n_window_elimination`). row_number() … <= k becomes a grouped arg_min/arg_max of k values.
  Example: `row_number() OVER (PARTITION BY g ORDER BY v) <= 3`
  Faster because: No sort of each partition, just a hash aggregate.

- **window_self_join** (new; switch: `window_self_join`). A window aggregate over the whole partition becomes a GROUP BY joined back to the rows.
  Example: `sum(x) OVER (PARTITION BY k)`
  Faster because: A hash aggregate is cheaper than a partitioned window.

- **window_join_pushdown** (new; switch: `window_join_pushdown`). Moves a join below a window when that keeps the partitions intact.
  Example: `window, then a filtering join on its partition key`
  Faster because: The window runs on fewer rows.

- **row_number_rewriter** (new; switch: `window_rewriter`). row_number() OVER () on a table becomes the scan's row number column.
  Example: `SELECT row_number() OVER (), * FROM t`
  Faster because: No window operator at all.

- **remove_redundant_order** (new; switch: `redundant_order`). Drops an ORDER BY when nothing above it depends on the order.
  Example: `an ORDER BY inside a subquery that is then joined`
  Faster because: The sort is skipped.

- **redundant_order_keys** (new; switch: `redundant_order_keys`). Drops sort keys that earlier keys already determine.
  Example: `ORDER BY id, name   -- id is the primary key`
  Faster because: Fewer keys to compare.

### Avoid repeated work (compute once)

- **common_subplan** (new; switch: `common_subplan`). A subplan that appears more than once runs once, as a materialized CTE.
  Example: `a view or CTE read twice in one query`
  Faster because: Computed once.

- **cte_inlining** (new; switch: `cte_inlining`). A CTE read once, or one that is cheap, is inlined in place of its scans.
  Example: `WITH x AS (…) SELECT … FROM x WHERE …`
  Faster because: Filters can then be pushed into what was the CTE.

### Read and move fewer bytes (narrower scans and values)

- **unused_columns** (changed; switch: `unused_columns`). Columns nothing reads are not scanned or copied through joins.
  Example: `SELECT a FROM wide_table`
  Faster because: Less I/O and narrower rows in every operator.

- **extract_pushdown** (new; switch: `unused_columns`). Reading s.a.b or v.x::INT scans only that field of a STRUCT or VARIANT, including shredded VARIANT in Parquet.
  Example: `SELECT v.price::DOUBLE FROM events`
  Faster because: Only that field is read, not the whole value.

- **type_pushdown** (changed; switch: `type_pushdown`). A cast of a CSV column moves into the CSV reader.
  Example: `SELECT num::HUGEINT FROM read_csv('f.csv')`
  Faster because: The reader parses straight into the target type.

- **compressed_materialization** (new; switch: `compressed_materialization`). Sorts, groups and joins on narrower values when statistics show they fit.
  Example: `INTEGER 300..309  →  1 byte;  short strings  →  integers`
  Faster because: Less memory, and faster comparisons in sorts and hash tables.

- **projection_placement** (new; switch: `projection_placement`). Computes an expression on whichever side of a join has fewer rows.
  Example: `upper(name) before or after a join`
  Faster because: The expression runs on fewer rows.

- **sampling_pushdown** (new; switch: `sampling_pushdown`). A SYSTEM sample of a scan without filters is taken by the scan, which skips whole vectors.
  Example: `FROM t USING SAMPLE 10%`
  Faster because: The rows left out of the sample are never read.

- **index_scan** (changed; switch: no separate switch). Uses the ART index when only a few rows match.
  Example: `WHERE pk = 42`
  Faster because: An index lookup instead of a full scan.

- **partitioned_execution** (new; switch: `partitioned_execution`). Splits a large aggregate, sort or window into ranges of one column, using row-group bounds.
  Example: `a large GROUP BY over a big table`
  Faster because: Each part fits in memory and the parts can run on their own.

## What changed in passes that were already there

Most of Stage 3 is new files. These passes existed before and were extended to match C++.

| File | Lines | What changed |
|---|---|---|
| `join_order.rs` | +1,986 | C++'s selectivity-based cardinality estimator, total and current column domains, SEMI/ANTI joins placed on the one relation they read, a greedy fallback for large queries, join domains restricted after key filters. |
| `statistics.rs` | +1,541 | Distinct counts, string prefixes and lengths, zone maps through constants, identities and functions, nested column elements, monotone function and cast statistics, perfect hash group-by. |
| `rewriter.rs` | +1,314 | Many more rewrite rules, e.g. date_trunc simplification, contains-to-IN, list comprehension rewrite, and division/modulo treated as able to fail. |
| `filter_pushdown.rs` | +473 | Filters now go through the FilterCombiner, which adds transitive filters. Join filters push through integer casts. |
| `scan_filters.rs` | +207 | Scan filter estimates from column statistics, and only filters that are safe to run first move into the scan. |
| `unused_columns.rs` | +128 | Works with late materialization and extract pushdown. A scan that reads no column scans the row id. |
| `deliminator.rs` | +59 | Handles inequality cases as C++ does. |
| `build_side.rs`, `type_pushdown.rs`, `index_scan.rs` | +15 | Small changes so they work with the new passes. |

## See a pass at work

Compare the plan with a pass on and off. The difference between the two is what that pass did.

```
EXPLAIN SELECT ...;                                   -- all passes on

SET disabled_optimizers = 'join_order,filter_pushdown';
EXPLAIN SELECT ...;                                   -- without those two

PRAGMA disable_optimizer;                             -- every pass off
```

Each pass is a port of the DuckDB C++ optimizer file named in its header comment. Where duckrs differs from C++, the header says so.
