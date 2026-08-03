# Task 7 audit fix report

Status: DONE (test-only)

## Verdict

Added `construct_value_preserves_engine_throw_origin`. Focused suite is 4/4 green.
No production edit.

## Gap

`nested_callback_construct_engine_type_error_is_intrinsic` raises a `construct`
TypeError inside an `Array.prototype.map` callback, so the throw crosses
`call_value`, not the runtime-constructor boundary in `construct_value`.
`construct_value`'s own `UncaughtThrow → ThrowValueOrigin` conversion was
therefore uncovered.

## Contract restored

File: `crates/bamts-runtime/src/lib.rs`

- `construct_value` maps `RuntimeErrorKind::UncaughtThrow { value, origin }` to
  `EvalFailure::ThrowValueOrigin { value, origin }` (approx. lines 6724–6730).
- New unit obtains a runtime closure constructor, drives
  `Machine::construct_value` directly, and asserts payload + origin exactly.

## New test

`tests::construct_value_preserves_engine_throw_origin`

- Allocates a runtime `HeapEntry::Function` whose body `Call`s non-callable
  `Int32(0)`, producing engine `ThrowOrigin::TypeError { operation: "call" }`.
- Invokes `machine.construct_value(callee, &[])`.
- Asserts:

```rust
Err(EvalFailure::ThrowValueOrigin {
    value: Value::UNDEFINED,
    origin: ThrowOrigin::TypeError { operation: "call" },
})
```

- `operation: "call"` is deliberate: the outer boundary is construct, so a
  leaked boundary error would read `"construct"`. Also asserts
  `machine.frames.is_empty()`.

## Focused verification

Command:

```bash
cargo test -p bamts-runtime --lib -- \
  native_callback_throw_is_caught_at_outer_call_site \
  nested_callback_call_engine_type_error_is_intrinsic \
  nested_callback_construct_engine_type_error_is_intrinsic \
  construct_value_preserves_engine_throw_origin
```

Result:

```text
running 4 tests
test tests::construct_value_preserves_engine_throw_origin ... ok
test tests::native_callback_throw_is_caught_at_outer_call_site ... ok
test tests::nested_callback_construct_engine_type_error_is_intrinsic ... ok
test tests::nested_callback_call_engine_type_error_is_intrinsic ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 429 filtered out; finished in 0.01s
```

| Concern | Test | Result |
|---|---|---|
| Nested call engine TypeError | `nested_callback_call_engine_type_error_is_intrinsic` | ok |
| Nested construct engine TypeError | `nested_callback_construct_engine_type_error_is_intrinsic` | ok |
| Explicit thrown-value callback | `native_callback_throw_is_caught_at_outer_call_site` | ok |
| Direct `construct_value` origin | `construct_value_preserves_engine_throw_origin` | ok |

## Scope

- Test-only addition in `mod tests`.
- Existing outer-catch tests kept unchanged.
- No production edit.
- Logical diff is the new unit only (inserted after
  `nested_callback_construct_engine_type_error_is_intrinsic`).
