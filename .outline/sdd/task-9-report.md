# Task 9 implementer report

Status: DONE

## Verdict

Async function rejections now normalize engine-origin failures through
`promise_rejection_value` so the rejected Promise stores a materialized
intrinsic Error, while bytecode-thrown values keep identity and Bytecode
origin. Exact tests green after one production fix in `settle_async_step`.

## Brief (authoritative)

`.outline/sdd/task-9-brief.md`:

- Goal: prove an async function rejection stores an observable intrinsic Error
  for an engine-origin failure, while preserving the identity of a value
  explicitly thrown by bytecode.
- Files: `crates/bamts-runtime/src/lib.rs` only.
- Exact tests:
  - `async_engine_type_error_rejection_has_intrinsic_shape`
  - `async_prematerialized_rejection_keeps_identity`
- Drive `start_async_call` / `promise_rejection_value`, not the normalizer
  directly. Keep `Throw(origin)` materializes once; `ThrowValue` /
  `ThrowValueOrigin` preserve values.
- Fix production only if the new tests fail.
- Commit: `fix(runtime): normalize async intrinsic rejections` when a fix
  is required.

## Production fix

`settle_async_step` `AsyncStep::Throw` previously called
`reject_promise(promise, value, origin)` with the raw uncaught payload, so an
engine TypeError rejected with `Value::UNDEFINED`.

It now converts the uncaught throw into the matching `EvalFailure` shape and
rejects through `reject_promise_failure` → `promise_rejection_value`:

- still-lazy engine origin (`UNDEFINED` + non-Bytecode) → `Throw(origin)` →
  materialize once
- Bytecode → `ThrowValue(value)`
- already-materialized engine value → `ThrowValueOrigin { value, origin }`

## Tests added

- `tests::async_engine_type_error_rejection_has_intrinsic_shape`
  Async runtime function calls a non-callable; rejected Promise reason is
  non-undefined, TypeError name/message/`instanceof`, origin
  `TypeError { operation: "call" }`.
- `tests::async_prematerialized_rejection_keeps_identity`
  Async runtime function throws a preallocated object argument; rejected
  reason is that exact Value with `ThrowOrigin::Bytecode`.

Both invoke via `call_value` → `start_async_call` using existing helpers
(`async_function`, `generator_callable`).

## Focused verification (exact evidence)

Exact tests:

```bash
cargo test -p bamts-runtime --lib -- \
  async_engine_type_error_rejection_has_intrinsic_shape \
  async_prematerialized_rejection_keeps_identity -- --exact
```

```text
running 2 tests
test tests::async_prematerialized_rejection_keeps_identity ... ok
test tests::async_engine_type_error_rejection_has_intrinsic_shape ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 433 filtered out; finished in 0.01s
```

Focused async rejection neighbors:

```bash
cargo test -p bamts-runtime --lib -- \
  promise_rejection \
  async_generator_returned_promise_rejection \
  async_generator_throw_rejects \
  async_generator_next_on_incompatible_receiver_rejects \
  async_await_setup_failure
```

```text
running 4 tests
test tests::async_generator_next_on_incompatible_receiver_rejects_promise ... ok
test tests::async_await_setup_failure_releases_suspended_registers ... ok
test tests::async_generator_returned_promise_rejection_rejects_front ... ok
test tests::async_generator_throw_rejects_front_and_drains_queue ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 431 filtered out; finished in 0.01s
```

(`promise_rejection` matched no dedicated unit name; coverage of
`promise_rejection_value` is exercised by the new engine TypeError exact test.)

## Scope boundaries

- Only `crates/bamts-runtime/src/lib.rs` edited.
- No project-wide suites.
- Other dirty workspace files left unstaged.
