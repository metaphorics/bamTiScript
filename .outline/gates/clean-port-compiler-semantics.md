# Clean port: compiler semantics

This gate tracks the recovery of the compiler binder, checker, and type-system semantics slice from the harvested plan.

## Scope

- Owned: `crates/bamts-compiler/src/{binder,checker}/**`, `crates/bamts-compiler/src/checker.rs`, and the dedicated tests under `crates/bamts-compiler/tests/`.
- Excluded: compiler roots and registries (`lib.rs`, `Cargo.toml`), scanner/parser/syntax/directives, emitter/project/CLI, and external subsystems.

## Gates

- [x] G1: every owned path is classified once and recorded.
  Evidence: `.outline/port-audit/compiler-semantics.md` lists 14 source paths and 2 test paths as ALREADY_HEAD, PORTED, or DEFERRED_CROSS_SLICE with a concrete reason for each.

- [x] G2: the four unique checker semantic submodules are placed in the current `checker` module tree.
  Evidence: `crates/bamts-compiler/src/checker.rs` now declares `conditional_types`, `decorators`, `enum_namespace`, and `overloads` using `#[path]` attributes that resolve to the existing files in `crates/bamts-compiler/src/checker/`.

- [x] G3: current JSX architecture and existing semantic APIs are preserved.
  Evidence: `checker.rs` retains its existing `pub mod jsx;` and re-exports `inference`, `narrowing`, and `relations`. No owned JSX logic, cancellation, or `TypeTable` API was changed.

- [ ] G4: the top-level `binder` export and merge tables are wired into the crate root.
  Evidence: deferred to the root owner because it requires adding `mod binder;` to `crates/bamts-compiler/src/lib.rs` and integrating `ExportTable`/`MergeTable` into the program-wide export/merge resolution. Recorded as DEFERRED_CROSS_SLICE in the audit.

- [x] G5: no stale deletion, markers, TODOs, placeholders, no-ops, or parallel representations were introduced in owned paths.
  Evidence: the only owned edits were the module declarations in `checker.rs`; all other owned files were either already on HEAD or are classified and left untouched pending cross-slice wiring.

## Notes for the root owner

The `crates/bamts-compiler/src/binder/` directory contains `mod.rs`, `exports.rs`, and `merging.rs`. When the compiler root is available, these should become `crate::binder` and the harvested `checker.rs` call patterns (e.g., `crate::binder::exports::ExportTable` and `crate::binder::merging::MergeTable`) should be restored. Until then, the current `checker.rs` remains unchanged and keeps its existing binder at `crate::checker::binder`.
