# Task 14 implementer report

Status: DONE

## Verdict

High-severity Promise rejection materialization defect is fixed at the single
`reject_promise` sink. Lazy engine-origin reasons (`UNDEFINED` + non-Bytecode
origin) materialize once into realm-intrinsic Errors before settlement;
already-materialized and Bytecode-origin reasons retain identity. Async
function, async generator body, incompatible receiver, failed import, and
direct sink identity/materialization tests are green.

## Blocking finding (resolved)

Engine-origin Promise rejections outside `settle_async_step` previously stored
`Value::UNDEFINED` with a non-Bytecode origin, so JavaScript handlers observed
`undefined` instead of an intrinsic Error. Paths affected:

- async generator body throws (`settle_async_generator_step` Throw arm)
- `AsyncGenerator.prototype.next` incompatible receiver
- failed-import / dynamic-import rejection forwarding
- any reaction that forwarded a still-lazy reason through `reject_promise`

## Production fix

`Machine::reject_promise` in `crates/bamts-runtime/src/lib.rs`:

- when `catch_value_needs_materialization(reason, origin)` holds and the
  promise is still pending, replace `reason` with
  `self.materialize_engine_origin(origin)?` before `settle_promise`
- already-settled promises remain no-ops and do not allocate a reason
- non-lazy / Bytecode reasons pass through unchanged

No call-site fan-out: async generators, incompatible receivers, import
replay, and reaction forwarding all inherit the sink contract. Existing
`settle_async_step` normalization remains harmless (post-materialize reasons
are non-`UNDEFINED`, so no double allocation).

## Tests

Added/strengthened in `crates/bamts-runtime/src/lib.rs`:

- `reject_promise_materializes_lazy_type_error_once` — direct sink; lazy
  TypeError materializes once; second reject on settled promise is a no-op
- `reject_promise_keeps_prematerialized_identity` — direct sink; concrete
  Bytecode reason keeps exact Value identity
- `async_generator_engine_type_error_rejection_has_intrinsic_shape` — body
  calls a non-callable; front `next()` capability rejects with intrinsic
  TypeError
- `async_generator_next_on_incompatible_receiver_rejects_promise` —
  strengthened from asserting `UNDEFINED` to asserting TypeError
  name/message/`instanceof` and matching origin

Existing green:

- `async_engine_type_error_rejection_has_intrinsic_shape`
- `async_prematerialized_rejection_keeps_identity`
- `failed_import_caught_by_dependent_is_intrinsic_type_error`
- `dynamic_import_rethrows_one_stored_failure_at_each_import_site`

## Verification

```bash
cargo test -p bamts-runtime --lib reject_promise_
cargo test -p bamts-runtime --lib async_engine_type_error_rejection_has_intrinsic_shape
cargo test -p bamts-runtime --lib async_prematerialized_rejection_keeps_identity
cargo test -p bamts-runtime --lib async_generator_engine_type_error_rejection_has_intrinsic_shape
cargo test -p bamts-runtime --lib async_generator_next_on_incompatible_receiver_rejects_promise
cargo test -p bamts-runtime --lib failed_import_caught_by_dependent_is_intrinsic_type_error
cargo test -p bamts-runtime --lib dynamic_import_rethrows_one_stored_failure_at_each_import_site
```

All listed filters: passed (0 failed).

Neighbors also green: `async_generator_throw_rejects_front_and_drains_queue`,
`async_generator_returned_promise_rejection_rejects_front`.

## Staging discipline

- Staged only `crates/bamts-runtime/src/lib.rs` and this report
  (force-added under ignored `.outline/sdd/`).
- Left unrelated dirty workspace files unstaged.

## Commit

- Message: `fix(runtime): materialize promise rejection reasons`
- High-severity findings remaining for Task 14 architecture brief: **0**
  (this blocking finding closed; medium/low from oracle/reviewer left for
  the final findings task).

## Self-review

- Single sink; no duplicated normalizers at each call site.
- Pending-check before materialization avoids heap churn on settled no-ops.
- Identity contracts for prematerialized / Bytecode reasons preserved.
- Prior Task 9 async-function path remains green without edits.
