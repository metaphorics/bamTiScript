# Clean port gate: bytecode, codegen, stdlib

## Scope

Owned surfaces: `crates/bamts-bytecode/**`, `crates/bamts-codegen/**`, `crates/bamts-stdlib/**`, native bytecode/codegen fixtures/tests inside those trees, and this slice's durable audit artifact. This gate is the bytecode/codegen sibling of `.outline/gates/clean-port-runtime-native.md`; it verifies that the produced instruction set and lowering layer match the single Suspend-mode ABI and dense helper table the runtime side already accepted.

## Manual gates

- [x] Every owned tracked stash delta and every owned current untracked addition is classified exactly once in `.outline/port-audit/bytecode-codegen.md`.
- [x] The owned current architecture has one suspension representation: `Instruction::Suspend { dst, src, resume, mode }` and `Instruction::Await { dst, src, resume }`, one CFG successor per suspension, and `RESUME_NEXT`/`RESUME_THROW`/`RESUME_RETURN` constants; no owned accepted delta restores `resume_return` or a parallel completion enum.
- [x] The current codegen helper table is dense at indices `0..=46`, with `Helper::ResumeMode` at external index `46`, matching `bamts_native::HELPER_COUNT = 47` in `crates/bamts-native/src/native_bridge.rs`.
- [x] Proposed helpers `47..=52` (`GetSuper`, `SetSuper`, `ImportAttributes`, `ImportDynamicAttributes`, `CopyDataProperties`, `GetTemplateObject`) and the bytecode tags `52..=57` that would consume them remain unintroduced; their compiler producers are absent from the current owned program and the runtime helper table stops at `46`.
- [x] The `aot` feature now enables `dep:bamts-native` so `crates/bamts-codegen/src/aot.rs` can read `bamts_native::HELPER_COUNT`; the rejected `sha2` and `target-lexicon` dependencies were not added.
- [x] Current `bamts-bytecode` decoder/encoder, verifier, and `bamts-codegen` lowering, JIT, and AOT consumers agree on the helper mapping and suspension contract; stale untracked helper/opcode validators and split modules are rejected rather than wired beside them.
- [x] Untracked extension modules (`isa.rs`, `verifier.rs`, `aot/*.rs`, `jit/helpers.rs`, `jit/tiering.rs`) that depend on an expanded helper set, absent AOT linking consumers, or no runtime installer are rejected rather than retained as dead parallel implementations.
- [x] Every accepted current path received implementation-completeness review: the Suspend/Await lowering emits token `P + 1` into `frame.bytecode_pc`, yields `src`, returns `Suspend`, and the resume prologue calls `Helper::ResumeValue` then `Helper::ResumeMode` (for `Suspend`) before continuing at `resume`.
- [x] Cross-slice candidates are named explicitly and left untouched pending a current producer/driver contract; no sibling-owned file was edited.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust build gates (`cargo check -p bamts-bytecode`, `cargo check -p bamts-codegen --features aot`, `cargo check -p bamts-codegen --features host-jit`).
- [ ] Runtime, bytecode, Interpreter/JIT/AOT, and native differential tests.
- [ ] Completion leaf, cluster, track, wave, root, mutation, and release gate checkers.
- [ ] Generated evidence and receipt regeneration.

## Evidence

- Inventory source: `git diff --name-status --find-renames stash@{0}^1 stash@{0}` and `git status --short --untracked-files=all`, both restricted to the owned pathspecs.
- Tracked inventory: 5 paths (1 `PORTED`, 4 `REJECTED_STALE`).
- Current untracked inventory: 8 paths (2 `REJECTED_INVALID`, 6 `REJECTED_STALE`).
- Missing owned tree: `crates/bamts-stdlib` (not present in harvest or HEAD).
- Total path accounting: 13 paths, each appearing once in `.outline/port-audit/bytecode-codegen.md`.
- Current ABI evidence: `crates/bamts-bytecode/src/lib.rs::Instruction::Suspend { dst, src, resume, mode }`, `Instruction::Await { dst, src, resume }`, `RESUME_NEXT`/`RESUME_THROW`/`RESUME_RETURN` constants, `visit_successors`; `crates/bamts-codegen/src/lib.rs::emit_suspend`, `emit_resume_prologue`, `Helper::ResumeMode`, and `register_offset` validation.
- Helper table evidence: `crates/bamts-codegen/src/lib.rs` `Helper` enum order up to `ResumeMode` (external index `46`); `crates/bamts-native/src/native_bridge.rs` `pub const HELPER_COUNT: u32 = 47`; `crates/bamts-codegen/src/aot.rs` reads `bamts_native::HELPER_COUNT`.
- Cargo feature fix evidence: `crates/bamts-codegen/Cargo.toml` `[features] aot = ["dep:bamts-native", "dep:cranelift-object"]`.
- Parent validation remains intentionally pending by assignment.
