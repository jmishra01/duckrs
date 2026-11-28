# pintail-common

`pintail-common` holds the building blocks that every other pintail crate uses: the SQL type
system (`LogicalType`, `PhysicalType`), single values (`Value`), DuckDB's 16-byte string
(`string_t`), columnar vectors and data chunks, the vectorized executors, the row format used by
hash tables and sorting, the columnar buffer of results, sort keys, the binary serializer, errors,
and helpers for VARIANT, GEOMETRY and JSON values. It is a port of `duckdb/src/common` plus a few
pieces from elsewhere (see the layout table). It is the bottom of the stack: it depends on no other
workspace crate, and `pintail-parser`, `pintail-core`, `pintail-main`, `pintail` and `pintail-cli` all
depend on it. See [`../../docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the whole stack.

## At a glance

| | |
|---|---|
| Lines of Rust (`src/`) | 19,970 |
| Main entry points | `types::LogicalType`, `value::Value`, `vector::{Vector, DataChunk, SelectionVector, ValidityMask}`, `executor::{unary, binary, select, ...}`, `row::TupleDataCollection`, `column_data::ColumnDataCollection`, `sort_key::create_sort_keys`, `serializer::{BinarySerializer, BinaryDeserializer}`, `Error` / `ErrorKind` / `Result` |
| C++ it ports | `src/common/types`, `src/common/types/row`, `src/common/types/column`, `src/common/vector_operations`, `src/common/row_operations`, `src/common/serializer`, `src/common/operator`, plus `src/function/scalar/create_sort_key.cpp`, `src/function/cast/variant`, `src/storage/storage_info.cpp`, parts of `extension/json` and `third_party/fmt` / `utf8proc` |

## Dependencies

Workspace crates: none.

External crates (`Cargo.toml`):

| Crate | Used for |
|---|---|
| `hashbrown` | `RowKeyMap` / `RowKeySet` in `sort_key.rs`, which look up keys by borrowed bytes (`hashbrown::Equivalent`) |
| `parking_lot` | the `Mutex` inside `StringHeap` (`types/string_t.rs`) and `StringBuffer` (`vector/buffer.rs`) |
| `proptest` (dev) | property tests of vectors, executors, rows, sort keys, the serializer and dates |
| `criterion` (dev) | the `executor` bench |

## Layout

| File | What it does | Ported from (`duckdb/...`) |
|---|---|---|
| [`src/lib.rs`](src/lib.rs) | Module list, `STANDARD_VECTOR_SIZE` (2048), `prefetch` | |
| [`src/error.rs`](src/error.rs) | `Error`, `ErrorKind` (mirrors `ExceptionType`), `Result` | `src/common/exception.cpp` |
| **`src/types/`** | | |
| [`types/mod.rs`](src/types/mod.rs) | `LogicalTypeId`, `LogicalType`, `PhysicalType`, decimal helpers, aggregate-state types | `src/common/types.cpp` |
| [`types/native.rs`](src/types/native.rs) | `NativeType` (Rust types a vector stores directly), `ListEntry` (`list_entry_t`) | `include/duckdb/common/types.hpp` |
| [`types/string_t.rs`](src/types/string_t.rs) | `StringT` and the `StringHeap` arena | `include/duckdb/common/types/string_type.hpp`, `string_heap.cpp` |
| `types/date.rs`, `time.rs`, `timestamp.rs`, `interval.rs`, `date_part.rs` | Date and time types: parsing, formatting, arithmetic | `src/common/types/{date,time,timestamp}.cpp`, `interval.hpp`, `enums/date_part_specifier.cpp` |
| `types/uuid.rs`, `bit.rs`, `bignum.rs` | UUID, BIT and BIGNUM values | `src/common/types/{uuid,bit,bignum}.cpp` |
| `types/identifier.rs` | `sql_identifier`: quotes a name when needed; the keyword check is set by the parser | `src/parser/keyword_helper.cpp` |
| [`src/value.rs`](src/value.rs) | `Value` and `ValueData`: one typed SQL value | `src/common/types/value.cpp` |
| **`src/vector/`** | | |
| [`vector/mod.rs`](src/vector/mod.rs) | `Vector`, `VectorType`, `UnifiedFormat` | `src/common/types/vector.cpp` |
| [`vector/buffer.rs`](src/vector/buffer.rs) | `DataBuffer` (aligned value storage), `StringBuffer` | `src/common/types/vector_buffer.cpp` |
| [`vector/validity.rs`](src/vector/validity.rs) | `ValidityMask`: one bit per row | `include/duckdb/common/types/validity_mask.hpp` |
| [`vector/selection.rs`](src/vector/selection.rs) | `SelectionVector` | `include/duckdb/common/types/selection_vector.hpp` |
| [`vector/chunk.rs`](src/vector/chunk.rs) | `DataChunk` | `src/common/types/data_chunk.cpp` |
| [`vector/copy.rs`](src/vector/copy.rs) | Copying rows between vectors; `StringCopy` (deep or by reference) | `src/common/vector_operations/vector_copy.cpp` |
| [`src/executor.rs`](src/executor.rs) | Unary, binary and select executors | `include/duckdb/common/vector_operations/{unary,binary,scalar}_executor.hpp` |
| **`src/row/`** | | |
| [`row/layout.rs`](src/row/layout.rs) | `TupleDataLayout`: where each column and aggregate state sits in a row | `src/common/types/row/tuple_data_layout.cpp` |
| [`row/collection.rs`](src/row/collection.rs) | `TupleDataCollection`, `RowPtr`: rows in DuckDB's row format, scatter and gather | `src/common/types/row/tuple_data_collection.cpp`, `tuple_data_scatter_gather.cpp` |
| [`row/matcher.rs`](src/row/matcher.rs) | `RowMatcher`: compares chunk columns with stored rows | `src/common/row_operations/row_matcher.cpp` |
| [`src/column_data.rs`](src/column_data.rs) | `ColumnDataCollection`: append-only chunked columnar buffer | `src/common/types/column/column_data_collection.cpp` |
| [`src/sort_key.rs`](src/sort_key.rs) | Byte-comparable sort keys, `RowKey`, `sort_chunk`, `sort_collection` | `src/function/scalar/create_sort_key.cpp`, `include/duckdb/common/radix.hpp` |
| [`src/comparison.rs`](src/comparison.rs) | `DuckOrd` (DuckDB's total order, NaN included), `ComparisonType` | `include/duckdb/common/operator/comparison_operators.hpp` |
| [`src/hash.rs`](src/hash.rs) | Hashes that match DuckDB bit for bit; `hash_vector`, `combine_hash` | `src/common/types/hash.cpp`, `vector_operations/vector_hash.cpp` |
| [`src/serializer.rs`](src/serializer.rs) | DuckDB's binary format (and its JSON form): `BinarySerializer`, `BinaryDeserializer`, `Serialize`, `Deserialize` | `src/common/serializer/*`, `src/storage/serialization/serialize_types.cpp` |
| [`src/storage_version.rs`](src/storage_version.rs) | `StorageVersion`: the file format version being written | `src/storage/storage_info.cpp` |
| [`src/variant.rs`](src/variant.rs), [`variant/shredding.rs`](src/variant/shredding.rs) | VARIANT values (`VariantNode`) and shredding | `src/function/cast/variant/to_variant.cpp`, `src/storage/table/variant/variant_shredding.cpp` |
| [`src/geometry.rs`](src/geometry.rs), [`src/geometry_crs.rs`](src/geometry_crs.rs) | GEOMETRY as WKB: WKT in and out, extents; coordinate reference systems | `src/common/types/geometry.cpp`, `geometry_crs.cpp` |
| [`src/json.rs`](src/json.rs) | A JSON value with yyjson's reading rules | `extension/json` |
| [`src/string_cast.rs`](src/string_cast.rs), [`src/fmt_float.rs`](src/fmt_float.rs) | Number, decimal and blob to text, with the digits DuckDB's bundled fmt prints | `src/common/operator/string_cast.cpp`, `third_party/fmt` |
| [`src/string_util.rs`](src/string_util.rs) | "Did you mean" candidates for error messages | `src/common/string_util.cpp` |
| [`src/render_width.rs`](src/render_width.rs) | Display width and grapheme clusters for the box renderer (approximate) | `third_party/utf8proc/utf8proc_wrapper.cpp` |
| [`src/path.rs`](src/path.rs) | Path parsing and joining (not the Windows-only parts) | `src/common/path.cpp` |

## Key types and APIs

- **`LogicalType`** is a SQL type, such as `INTEGER` or `LIST(VARCHAR)`. Type parameters live behind
  an `Arc`, so cloning is cheap. `physical_type()` gives the **`PhysicalType`**, the way values are
  stored (`Int32`, `Varchar`, `List`, ...). Its discriminants match the C++ enum.
- **`Value`** is one typed value. Use it for constants and in cold paths. Execution works on vectors.
- **`NativeType`** is a sealed `unsafe` trait for the Rust types a flat vector can store
  (`bool`, the integers, `f32`/`f64`, `Interval`, `StringT`, `ListEntry`, ...). Typed access to a
  vector (`data::<T>()`) checks that `T::PHYSICAL` matches the vector's physical type.
- **`Vector`** is a column of up to `capacity` values. **`DataChunk`** is a set of equally long
  vectors, normally of `STANDARD_VECTOR_SIZE` rows.
- **`ValidityMask`** tracks NULLs, one bit per row, with no bitmap at all when every row is valid.
- **`SelectionVector`** maps output row `i` to a source row: `Identity`, `Zero` (every row reads
  row 0), `Explicit` or `Window` (a shared part of an explicit one).
- **`UnifiedFormat`** (from `Vector::to_unified`) is a row-addressable view of any vector: row `i`
  is at `data[sel.get(i)]`.
- **Executors** (`executor.rs`): `unary`, `binary`, their `_nullable` forms (the function returns
  `Option`, `None` gives NULL) and `_fallible` forms (the function returns `Result`), string forms
  (`unary_str`, `unary_to_str`, `unary_str_to_str`, `binary_str`), and `select` / `select_binary`,
  which return the matching rows as a `SelectionVector`.
- **`TupleDataCollection`** stores rows in DuckDB's row format, for hash tables, aggregation and
  sorting. **`RowMatcher`** checks hash-table hits against stored rows.
- **`ColumnDataCollection`** is an append-only, chunked columnar buffer. Results, CTEs and
  in-memory tables are materialized into one.
- **Sort keys** (`create_sort_keys`, `create_row_keys`, `sort_chunk`, `sort_collection`) encode rows
  as byte strings whose byte order equals the SQL order.
- **`BinarySerializer` / `BinaryDeserializer`** read and write DuckDB's binary format.
- **`Error`** has an `ErrorKind` and a message, plus an optional byte `position` in the query. It
  prints as `"<Kind> Error: <message>"`, the same as C++.

Example (checked against the current signatures):

```rust
use pintail_common::executor::{binary, select};
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use pintail_common::vector::{Vector, VectorType};

let mut a = Vector::new(LogicalType::INTEGER, 4);
a.data_mut::<i32>().copy_from_slice(&[1, 2, 3, 4]);
a.set_null(2, true);
let three = Vector::constant(&Value::integer(3));
assert_eq!(three.vector_type(), VectorType::Constant);

// sum = a + 3; the NULL in row 2 stays NULL and the closure is not called for it
let mut sum = Vector::new(LogicalType::INTEGER, 4);
binary(&a, &three, &mut sum, 4, |x: i32, y: i32| x + y);
assert_eq!(sum.data::<i32>()[..2], [4, 5]);
assert!(!sum.is_valid(2));

// the rows where sum > 4: rows 1 and 3
let (sel, n) = select(&sum, 4, |x: i32| x > 4);
assert_eq!((n, sel.get(0), sel.get(1)), (2, 1, 3));
assert_eq!(sum.get_value(3), Value::integer(7));
```

## How it works

### Vector representations

A `Vector` has a logical type and one of four representations (`VectorType`):

| `VectorType` | Stores | Row `i` is |
|---|---|---|
| `Flat` | one value per row, a validity mask, and side data | value `i` |
| `Constant` | one value (same storage as flat, one row) | value 0 |
| `Dictionary` | a `SelectionVector` and a child vector | child row `sel.get(i)` |
| `Sequence` | `start` and `increment` (integer types up to 8 bytes) | `start + i * increment` |

Flat and constant vectors keep their side data in an `Aux` value: a `StringBuffer` for
VARCHAR/BLOB, a child vector and its used size for LIST/MAP (each row is a `ListEntry`
`{offset, length}` into the child), child vectors for STRUCT, and one child of
`capacity * array_size` rows for ARRAY.

C++ also has FSST and shredded vectors. The code comment says these storage encodings come with
the storage engine. For now there is only a `shredded` flag: a flat VARIANT read from shredded
storage answers `is_shredded()`.

Some operations keep the representation instead of copying:

- `slice(sel, count)` leaves a constant vector as it is, composes the selection of a dictionary,
  and wraps any other vector in a new dictionary.
- `range(start, count)` shares a dictionary's child and a window of its selection, moves the start
  of a sequence, and for flat strings copies only the 16-byte `StringT`s, keeping the source's
  string buffer alive.
- `flatten(count)` turns any representation into a flat vector. `to_unified(count)` gives a
  `UnifiedFormat` view without changing the vector (a sequence is flattened into a copy).

### Copy-on-write through `Arc`

Cloning a `Vector` is cheap. The value storage is an `Arc<DataBuffer>`, the validity bitmap an
`Arc<Vec<u64>>`, and the string bytes an `Arc<StringBuffer>`, so a clone shares all of them. Every
write path (`data_mut`, `number_bytes_mut`, `string_data_mut`, `set_value`, `resize`, validity
writes) goes through `Arc::make_mut`. That copies the buffer first if it is shared, so clones
never see each other's writes. `LogicalType` shares its parameters the same way, and equality
first checks whether both types point at the same `Arc`.

`DataBuffer` is a `Vec<u128>`, so it is 16-byte aligned and zero-filled. It can be viewed as a
slice of any `NativeType`; that cast is one of the crate's `unsafe` blocks.

### `string_t`

`StringT` is DuckDB's 16-byte string, `#[repr(C)]`:

```text
bytes 0..4   length (u32)
bytes 4..8   first four bytes of the string (the prefix)
bytes 8..16  the rest of a string of up to 12 bytes, inlined
             or, for a longer string, a pointer to its bytes
```

Strings of up to 12 bytes (`INLINE_LENGTH`) live entirely in the 16 bytes; the padding is zeroed
so equal strings have equal bytes. Longer strings point at bytes owned elsewhere, usually the
`StringHeap` inside the vector's `StringBuffer`. The heap is an append-only arena: bytes never
move once added, so pointers stay valid until it is dropped. Comparisons look at the length and
prefix first and only follow the pointer when they must.

A `StringT` does not keep its bytes alive, so `StringT::as_bytes` and `StringT::from_raw_parts`
are `unsafe`. Safe code reads strings through `Vector::str_at` or `UnifiedFormat::str_at`, which
tie the bytes to the vector's or format's lifetime. When strings are copied from another vector
without their bytes (`StringCopy::Reference`), the target's `StringBuffer` records the source
buffer in `keep_alive`, so the bytes live as long as the target. `StringCopy::Deep` copies the
bytes instead, which is what `ColumnDataCollection` uses so it owns everything it holds.

### Executors

The executors apply a function to every row of one or two vectors. A constant input (or two)
gives a constant result. Flat inputs run a tight loop that reads the validity mask 64 rows at a
time, so all-valid and all-NULL entries skip the per-row bit test. Anything else goes through
`UnifiedFormat`. A NULL input gives a NULL output without calling the function.

### Row format

`TupleDataLayout` describes a row as

```text
[validity bits][heap size?][column 0][column 1]...[pad][aggregate 0][aggregate 1]...[pad]
```

`TupleDataCollection` keeps rows in fixed 256 KiB blocks that never move, so the `RowPtr`s that
hash tables store stay valid until the collection is reset or dropped. Reading through a
`RowPtr` is `unsafe` for that reason. It is in memory only; spilling comes with the storage
engine. One deviation from C++, written in the module comment: LIST and ARRAY values go to a
per-column side store, and the row holds an index into it, instead of a per-row heap. The row
format is never persisted, so only the behavior has to match.

### Sort keys and hashing

A sort key is a byte string whose `memcmp` order is the SQL order of the encoded values: each value
is a NULL/valid byte followed by its encoding, and DESC flips the bytes. Sorting by several
ORDER BY columns becomes a byte comparison. `RowKey` keeps keys of up to 30 bytes inline, for
grouping and distinct lookups. `hash.rs` reproduces DuckDB's hash values exactly, because they
decide hash-table and partition placement and `hash()` shows them to SQL.

### Serializer

DuckDB's binary format, used for the WAL, checkpoints and plan caches: an object is a list of
properties ended by field id `0xFFFF`; a property is a little-endian `u16` field id and its value;
integers are LEB128 varints; properties equal to their default are left out. The same serializer
can write DuckDB's JSON form.

## Status and gaps

- `VectorType` has no FSST or shredded variants yet (comment on `VectorType`).
- `ColumnDataCollection` and `TupleDataCollection` are in memory only; spilling through the
  buffer manager is not ported (module comments).
- VARIANT values are a self-contained byte string here, not DuckDB's nested columnar layout
  (`variant.rs` header). Some types report `storing {ty} in a VARIANT is not yet ported to pintail`.
- `render_width.rs` approximates utf8proc's tables, because the crate has no Unicode-table
  dependency.
- Errors that mark smaller gaps: `converting {} values to VARCHAR is not supported yet`
  (`value.rs`), `hashing {other} vectors ... is not supported yet` (`hash.rs`), some type info and
  type expressions in the deserializer, and non-equality comparisons of STRUCT rows in
  `RowMatcher`.
- `docs/REMAINING_WORK.md` item 52 ("Miri and ThreadSanitizer jobs in CI") is still open. The Miri
  job exists in CI (below); ThreadSanitizer does not.

## Testing

```sh
cargo test -p pintail-common              # unit, property and integration tests
cargo bench -p pintail-common             # Criterion bench (benches/executor.rs)
cargo +nightly miri test -p pintail-common   # what the CI Miri job runs
```

- Unit tests sit in `#[cfg(test)]` modules in most files, with larger suites in
  `src/vector/tests.rs`, `src/executor/tests.rs`, `src/row/tests.rs`, `src/sort_key/tests.rs` and
  `src/serializer/tests.rs`.
- Property tests (`proptest`) check that vector copies, unary executors and row scatter/gather
  agree with row-by-row results, that sort-key order matches SQL order and agrees with row keys,
  that serialized primitives round trip, and that dates round trip. Failing cases found in the past
  are kept in [`proptest-regressions/`](proptest-regressions/) and run again first.
- [`tests/state_type_roundtrip.rs`](tests/state_type_roundtrip.rs) checks that an aggregate-state
  type keeps its function through the serializer.
- [`benches/executor.rs`](benches/executor.rs) times `binary` (flat + flat, flat + constant, 10%
  NULLs), `binary_nullable` with a checked add, and `select` on one 2048-row vector.
- CI (`.github/workflows/ci.yml`) has a `miri` job: `cargo miri test -p pintail-common` on nightly
  with `PROPTEST_CASES=8` and `MIRIFLAGS=-Zmiri-disable-isolation`. It checks the crate's `unsafe`
  code (string_t, vector buffers, rows) for undefined behavior.
- Most behavior is also checked end to end by the sqllogictest corpus (`tools/sqllogictest`).

## Notes for contributors

- **`unsafe` lives here.** By `../CLAUDE.md`, new `unsafe` is allowed only in this crate's
  `vector/`, `row/`, `types/string_t.rs` and `types/native.rs` (and in `pintail-core/src/storage/`).
  Other crates should add a small safe API here and call it. Today the `unsafe` in this crate is in
  `types/string_t.rs` (the union, raw parts, `Send`/`Sync`), `types/native.rs` (`NativeType`),
  `vector/buffer.rs` (typed views of `DataBuffer`), `vector/mod.rs` and `vector/copy.rs` (writing
  `StringT`s), `row/collection.rs` and `row/matcher.rs` (row pointers), plus a prefetch in
  `lib.rs` and one in `hash.rs`.
- **SAFETY comments.** Every `unsafe` block gets a `// SAFETY:` comment just above it saying why it
  is sound, and every `unsafe fn` a `# Safety` section. The workspace lints enforce this
  (`clippy::undocumented_unsafe_blocks`, `unsafe_op_in_unsafe_fn = "deny"`), and CI runs clippy with
  `-D warnings`.
- **Stay Miri-clean.** Run `cargo miri test -p pintail-common` after touching `unsafe` code.
- **Typed kernels.** New kernels should work on typed data (`data::<T>()`, `data_mut::<T>()`, the
  executors, `UnifiedFormat`), not `Vector::get_value` / `set_value` per row. `Value` per row is
  fine in cold paths.
- **Writing strings.** `data_mut` refuses VARCHAR on purpose. Use `set_str` / `set_strs`, or
  `string_data_mut` in audited code, and register borrowed buffers with `keep_strings_alive`.
- **Port headers.** Each file starts with `// Ported from duckdb/<path> @ rust-port-baseline`;
  keep it when adding or moving code. Doc comments name the C++ function in backticks.
- **Match C++ output.** Error messages, `ErrorKind` names, hashes, float digits and sort orders
  must match DuckDB, because the sqllogictest corpus compares them exactly.
