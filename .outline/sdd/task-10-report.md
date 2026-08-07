# Task 10 audit fix report

Status: DONE

## Verdict

`raise_type_error` is now total: it always loads a fresh known non-callable
(`Constant::Boolean(true)`) and issues a bare `Call`, never invoking a
user-returned value. Callable invalid auto-accessor decorator returns TypeError
in Interpreter/JIT/AOT with side-effect count zero, matching TypeScript+Node.

## Brief (authoritative)

`.outline/sdd/task-10-brief.md` Audit fix:

- Remove the offender parameter from `raise_type_error`.
- Helper loads a fresh known non-callable constant and issues the bare Call.
- Update every call site.
- Add `invalid_auto_accessor_decorator_return_matches_tsc_oracle_in_every_execution_mode`.
- Commit: `fix(compiler): make bare TypeError throws total`.

## Gap

`apply_auto_accessor_decorators` rejected non-object decorator returns by
passing the returned value to `raise_type_error`. When that return was a
function, the helper `Call`ed it, running user code instead of throwing an
engine-origin TypeError.

## Production fix

File: `crates/bamts-compiler/src/lower.rs`

```rust
fn raise_type_error(
    &mut self,
    builder: &mut ModuleBuilder,
    range: TextRange,
) -> Result<(), LowerError> {
    // Always call a fresh known non-callable. Never pass a user-returned
    // value here: a callable invalid decorator return would run user code
    // instead of throwing an engine TypeError.
    let dummy = self.load_constant(builder, Constant::Boolean(true), range)?;
    let undefined = self.undefined(builder, range)?;
    let _ = self.call_with_registers(range, dummy, undefined, &[])?;
    Ok(())
}
```

Updated call sites (5):

- `accept_replacement_callable` — drop `returned`
- `collect_optional_callable` — drop `returned`
- closed `addInitializer` arm — drop redundant `closed_dummy` load; helper owns it
- open `addInitializer` bad-callback arm — drop `callback`
- `apply_auto_accessor_decorators` — drop `returned` (the audit hole)

Instruction shape unchanged: still bare `Call` of a non-callable, no
`LoadGlobal("TypeError")`, no `Construct`, no `Throw`.

## New differential

`invalid_auto_accessor_decorator_return_matches_tsc_oracle_in_every_execution_mode`
in `crates/bamts-verification/tests/corpus_differential.rs`:

- Auto-accessor decorator returns a function that increments `sideEffects` if
  invoked.
- TypeScript+Node and BamTS Interpreter/JIT/AOT catch `TypeError`.
- Observable output includes `sideEffects:0` (returned callable never invoked).

## Focused verification (exact evidence)

Instruction-shape:

```bash
cargo test -p bamts-compiler --lib raise_type_error
cargo test -p bamts-compiler --lib escaped_add_initializer_after_close_takes_type_error_path_not_append
```

```text
test lower::tests::raise_type_error_is_a_bare_throw_without_constructor_lookup ... ok
test result: ok. 1 passed; 0 failed; … 507 filtered out

test lower::tests::escaped_add_initializer_after_close_takes_type_error_path_not_append ... ok
test result: ok. 1 passed; 0 failed; … 507 filtered out
```

Named differentials (all-mode oracle):

```bash
cargo test -p bamts-verification --test corpus_differential \
  invalid_class_decorator_return_matches_tsc_oracle_in_every_execution_mode -- --exact
cargo test -p bamts-verification --test corpus_differential \
  invalid_auto_accessor_decorator_return_matches_tsc_oracle_in_every_execution_mode -- --exact
```

```text
test invalid_class_decorator_return_matches_tsc_oracle_in_every_execution_mode ... ok
test result: ok. 1 passed; 0 failed; … 45 filtered out

test invalid_auto_accessor_decorator_return_matches_tsc_oracle_in_every_execution_mode ... ok
test result: ok. 1 passed; 0 failed; … 45 filtered out
```

## Contract map

| Contract | Evidence | Status |
|---|---|---|
| `raise_type_error` loads internal non-callable and bare-Calls it — no offender param | `lower.rs` helper + call sites | PASS |
| Instruction shape still bare Call / no TypeError global / no Construct / no Throw | `raise_type_error_is_a_bare_throw_without_constructor_lookup` | PASS |
| Closed `addInitializer` still takes type-error path | `escaped_add_initializer_after_close_takes_type_error_path_not_append` | PASS |
| Invalid class decorator return still matches tsc/Node in every mode | `invalid_class_decorator_return_matches_tsc_oracle_in_every_execution_mode` | PASS |
| Callable invalid auto-accessor return TypeErrors; side effect count stays 0 | `invalid_auto_accessor_decorator_return_matches_tsc_oracle_in_every_execution_mode` | PASS |

## Scope boundaries

- Staged only `lower.rs`, the new differential, and this report.
- Other dirty tree work left unstaged.
- Native materialization remains Task 11.

## Commit

- Message: `fix(compiler): make bare TypeError throws total`

## Result

DONE.
