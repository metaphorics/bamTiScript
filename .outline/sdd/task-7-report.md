# Task 7 implementer report

Status: DONE (current-state, no commit, no source edit)

## Verdict

Engine-origin TypeErrors raised inside nested runtime callbacks retain origin
through `call_value` and `construct_value` boundaries and materialize as
intrinsic realm TypeError objects at the outer JavaScript catch. Explicit thrown
values are preserved verbatim across the native callback boundary. Focused tests
all pass. No edit required.

## Brief (authoritative)

`.outline/sdd/task-7-brief.md`:

- Goal: prove engine-origin TypeErrors raised inside nested runtime callbacks
  keep their origin through `call_value` and `construct_value` callback
  boundaries and materialize correctly at the outer JavaScript catch.
- Files: inspect `crates/bamts-runtime/src/lib.rs`; do not edit unless an exact
  focused test fails.
- Contract:
  - `nested_callback_call_engine_type_error_is_intrinsic` catches an intrinsic
    TypeError whose message identifies `call`.
  - `nested_callback_construct_engine_type_error_is_intrinsic` catches an
    intrinsic TypeError whose message identifies `construct`.
  - Both values are non-undefined and pass `instanceof` against the realm
    TypeError.
  - `native_callback_throw_is_caught_at_outer_call_site` preserves an explicit
    thrown value exactly.
  - Inspect `RuntimeErrorKind::UncaughtThrow { value, origin }` to
    `EvalFailure::ThrowValueOrigin` conversions in `call_value` and
    `construct_value`.
- Verification: focused interpreter tests only; imports, async jobs, lowerer
  bare errors, and native linked backends remain separate tasks.

## Contract locations inspected

File: `crates/bamts-runtime/src/lib.rs`

- `construct_value` UncaughtThrow conversion (approx. lines 6725–6728):
  `RuntimeErrorKind::UncaughtThrow { value, origin }` maps to
  `EvalFailure::ThrowValueOrigin { value, origin }`, preserving payload and
  origin across the construct callback boundary.
- `call_value` UncaughtThrow conversion (approx. lines 6844–6847):
  identical `UncaughtThrow` → `ThrowValueOrigin` preservation across the call
  callback boundary.
- Focused tests (approx. lines 17833, 17907, 18009):
  `native_callback_throw_is_caught_at_outer_call_site`,
  `nested_callback_call_engine_type_error_is_intrinsic`,
  `nested_callback_construct_engine_type_error_is_intrinsic`.

## Focused verification (exact evidence)

Command run:

```bash
cargo test -p bamts-runtime --lib -- \
  nested_callback_call_engine_type_error_is_intrinsic \
  nested_callback_construct_engine_type_error_is_intrinsic \
  native_callback_throw_is_caught_at_outer_call_site
```

Result:

```text
running 3 tests
test tests::native_callback_throw_is_caught_at_outer_call_site ... ok
test tests::nested_callback_call_engine_type_error_is_intrinsic ... ok
test tests::nested_callback_construct_engine_type_error_is_intrinsic ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 429 filtered out; finished in 0.01s
```

| Concern | Test | Result |
|---|---|---|
| Nested call engine TypeError | `tests::nested_callback_call_engine_type_error_is_intrinsic` | ok |
| Nested construct engine TypeError | `tests::nested_callback_construct_engine_type_error_is_intrinsic` | ok |
| Explicit thrown-value callback | `tests::native_callback_throw_is_caught_at_outer_call_site` | ok |

## Scope boundaries

- Interpreter-only verification in `lib.rs` as required by the brief.
- No project-wide checks.
- No commit.
- No lasting source edits (none needed; all focused tests green).

## Task 7 audit fix: construct_value origin unit

Status: DONE (test-only). See `.outline/sdd/task-7-fix-report.md`.

- Added `tests::construct_value_preserves_engine_throw_origin`.
- Drives `Machine::construct_value` directly; asserts
  `EvalFailure::ThrowValueOrigin { value: UNDEFINED, origin: TypeError { operation: "call" } }`.
- Focused suite 4/4 green. No production edit.
