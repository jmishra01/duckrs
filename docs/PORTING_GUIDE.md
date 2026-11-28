# Porting guide

How to turn DuckDB C++ into pintail Rust. The rule: **port behavior, not syntax.** Read the C++
for semantics and edge cases, then write idiomatic Rust.

## File headers

Every ported file starts with its source and the baseline marker:

```rust
// Ported from duckdb/src/common/types/date.cpp @ rust-port-baseline
```

## Idiom map

| C++ | Rust |
|---|---|
| `idx_t` | `usize` (`u64` in serialized structs) |
| `unique_ptr<T>` / `make_uniq` | `Box<T>` / `Box::new` |
| `shared_ptr<T>` | `Arc<T>` |
| `optional_ptr<T>` | `Option<&T>` (`Option<NonNull<T>>` only inside audited `unsafe`) |
| `reference<T>` | `&T` |
| raw back-pointers (child → parent) | `Weak<T>`, or an index into an owning arena |
| `vector<T>` | `Vec<T>` |
| `unordered_map` | `hashbrown::HashMap` |
| `case_insensitive_map_t` | a map keyed by a case-folding wrapper |
| `template <class OP>` with static `Operation` | generic `F: Fn(..)`, or a trait with zero-sized impls |
| `switch (type.InternalType())` dispatch | a `match` (or a dispatch macro) that calls a generic fn |
| class hierarchies for ASTs / logical plans / bound expressions | enums |
| `PhysicalOperator` and its states | traits, with `as_any()` downcasts where C++ uses `Cast<T>()` |
| `throw XException(...)` | `return Err(Error::new(ErrorKind::X, ...))` |
| `InternalException`, `D_ASSERT` | `panic!` / `debug_assert!` |
| `lock_guard<mutex>` | `parking_lot::Mutex` guard |
| `Serializer`/`Deserializer` with field ids | hand-written traits matching DuckDB's binary format (not serde) |

## Error messages

Copy them **verbatim**, including the `<Kind> Error: ` prefix that `pintail::Error`'s `Display`
adds. sqllogictest `statement error` blocks match message substrings, so a reworded message fails
tests. Find the C++ text with `grep -rn "message fragment" ../duckdb/src`.

## `unsafe`

Allowed only in kernels that need it (vector buffers, `string_t`, hash tables, compression,
SIMD). Every block gets a `// SAFETY:` comment (enforced by `clippy::undocumented_unsafe_blocks`),
and the module gets property tests plus a miri job in CI.

## Workflow for a module

1. Find the tests: `ls ../duckdb/test/sql/<area>`, and run them against the reference engine
   to see the expected behavior: `sqllogictest --engine reference test/sql/<area>`.
2. Port the C++ files, keeping the C++ file → Rust module mapping obvious.
3. Run `sqllogictest test/sql/<area>`, and fix failures until the target pass rate is reached.
4. Regenerate `tools/sqllogictest/known_passing.txt` so the passing files are protected.
