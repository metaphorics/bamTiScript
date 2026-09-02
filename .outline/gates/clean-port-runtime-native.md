# Clean port gate: runtime and native

## Scope

Owned surfaces: `crates/bamts-runtime/**`, `crates/bamts-vm/**`, `crates/bamts-bytecode/**`, `crates/bamts-codegen/**`, `crates/bamts-stdlib/**`, native runtime fixtures/tests inside those trees, runtime-only generated modules, and this slice's two durable audit artifacts.

## Manual gates

- [x] Every owned tracked stash delta and every owned current untracked addition is classified exactly once in `.outline/port-audit/runtime-native.md`.
- [x] Stash deletions of the active Date, JSON, and Uint8Array modules are rejected; current JSON CreateDataProperty and cached typed-array prototype/NewTarget semantics remain authoritative.
- [x] The owned current architecture has one suspension representation: `Suspend { dst, src, resume, mode }`, one CFG successor, and `ResumeCompletion::{Next, Throw, Return}`; no owned accepted delta restores `resume_return` or a parallel completion enum.
- [x] The current native helper table remains dense at indices `0..=46`, with `ResumeMode = 46` and `HELPER_COUNT = 47`; proposed helpers 47–52 remain unintroduced because their compiler producers are not in the current owned program.
- [x] Current interpreter, JIT, and AOT consumers agree on the helper mapping and suspension contract; stale untracked helper/opcode validators are rejected rather than wired beside them.
- [x] Untracked builtin, Intl, Temporal, VM, GC, and codegen additions that depend on removed APIs, absent heap variants, stale state machines, or no runtime installer are rejected rather than retained as dead parallel implementations.
- [x] Every accepted current path received implementation-completeness, expert-reread, defect-hunt, and free-polish review for abrupt completion, GC rooting, state transitions, bounds, and allocation behavior; no source delta survived those passes.
- [x] Cross-slice candidates are named explicitly and left untouched pending a current producer/driver contract; no sibling-owned file was edited.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust build gates.
- [ ] Runtime, bytecode, Interpreter/JIT/AOT, and native differential tests.
- [ ] Completion leaf, cluster, track, wave, root, mutation, and release gate checkers.
- [ ] Generated evidence and receipt regeneration.

## Evidence

- Inventory source: `git diff --name-status --find-renames stash@{0}^1 stash@{0}` and `git status --short --untracked-files=all`, both restricted to the owned pathspecs.
- Tracked inventory: 27 paths (23 `ALREADY_HEAD`, 4 `REJECTED_STALE`).
- Current untracked inventory: 52 paths (3 `ALREADY_HEAD`, 36 `REJECTED_STALE`, 7 `REJECTED_INVALID`, 6 `DEFERRED_CROSS_SLICE`).
- Total path accounting: 79 paths, each appearing once in `.outline/port-audit/runtime-native.md`.
- Current ABI evidence: `crates/bamts-bytecode/src/lib.rs::Instruction::Suspend`, `visit_successors`, `RESUME_NEXT`, `RESUME_THROW`, `RESUME_RETURN`; `crates/bamts-runtime/src/lib.rs::ResumeCompletion`; `crates/bamts-runtime/src/native.rs::HelperCall::ResumeMode`; `crates/bamts-codegen/src/lib.rs::emit_resume_prologue`.
- ECMAScript fix evidence retained: `crates/bamts-runtime/src/builtins/json.rs` uses `Machine::create_data_property_key`; `crates/bamts-runtime/src/builtins/uint8array.rs` installs the cached prototype and resolves `newTarget.prototype` with abrupt completion propagation.
- Parent validation remains intentionally pending by assignment.
