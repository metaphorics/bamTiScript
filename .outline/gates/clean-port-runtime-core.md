# Clean port gate: runtime core

## Scope

Owned surfaces: `crates/bamts-runtime/**` excluding `crates/bamts-runtime/src/builtins/**`;
`crates/bamts-vm/**` is not present in this checkout.

## Manual gates

- [x] Every owned tracked stash delta and every owned current untracked addition is classified exactly once in `.outline/port-audit/runtime-core.md`.
- [x] The `crates/bamts-runtime/src/vm/` pure-policy modules (`async_iterators`, `dynamic_import`, `esm_eval`, `explicit_resource`, `generator_async`, `import_attributes`) are wired into the build by declaring them in `crates/bamts-runtime/src/vm.rs`.
- [x] No owned accepted file contains `resume_return`, `ResumeReturn`, stale `TODO`/`FIXME`/`todo!`/`unimplemented!`, placeholder, or no-op markers.
- [x] The current `Instruction::Suspend` consumer shape (`Suspend { dst, src, resume, mode }`) is used in `crates/bamts-runtime/src/vm/generator_async.rs` tests; `resume_return` test fields were replaced with `mode: reg(2)`.
- [x] The runtime retains one `ResumeCompletion::{Next,Throw,Return}` state model; no parallel completion enum was introduced or accepted for the core state machine.
- [x] The active Date, JSON, and Uint8Array builtin modules remain untouched; no stash deletion of those files was applied.
- [x] Cross-slice candidates (`gc/stress_hardening.rs`, `intl/`, `temporal/`, all `builtins/` additions/deletions) are named in the audit and left untouched pending sibling slices.
- [x] No sibling-owned file was edited; changes are limited to `crates/bamts-runtime/src/vm.rs` and `crates/bamts-runtime/src/vm/generator_async.rs` within runtime-core.
- [x] No module/dependency-registry request from `RuntimeBuiltinsRecovery` was received for the owned `Cargo.toml`/`external_modules.rs` surfaces; none was integrated.

## Parent integration gates

Pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust build gates.
- [ ] Runtime test and differential gates.
- [ ] Completion/root/track/wave gate checkers.

## Evidence

- Audit: `.outline/port-audit/runtime-core.md`
- Wiring edit: `crates/bamts-runtime/src/vm.rs` lines 13-18 (submodule declarations)
- `resume_return` removal: `crates/bamts-runtime/src/vm/generator_async.rs` `Instruction::Suspend` fields
