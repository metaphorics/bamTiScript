# Clean port gate: authority and evidence

## Scope

Owned surfaces: root Rust, toolchain, workflow, and documentation configuration except root npm package files; `.github/**`; `bench/**`; `corpus/**`; `formal/**`; `proof/**`; `verification/**`; `crates/bamts-verification/**`; repository verification scripts and tools; and residual stash or untracked paths outside the compiler/CLI, runtime/native, and npm/package slices.

## Manual gates

- [x] Every owned root stash delta and current untracked addition is classified exactly once in `.outline/port-audit/authority-root.md`.
  - Evidence: `authority-root.md` contains 18 classified paths with measured totals (`ALREADY_HEAD` 7, `REJECTED_INVALID` 6, `PORTED` 5, `DEFERRED_CROSS_SLICE` 0). The full evidence ledger `.outline/port-audit/authority-evidence.md` is not owned by this root slice and remains pending for the evidence integrator.
- [ ] Every owned evidence and verification stash delta is classified exactly once in `.outline/port-audit/authority-evidence.md`.
  - Evidence: pending the evidence integrator; this slice did not edit `verification/**`, `proof/**`, `formal/**`, `bench/**`, `corpus/**`, `.github/**`, or `crates/bamts-verification/**`.
- [x] Current workflow, process-group cancellation, corpus, verification, formal, proof, and benchmark architecture remains authoritative.
  - Evidence: the root slice changed only `Cargo.toml`, `Cargo.lock`, and the three `.outline` gate/audit files; no workflow, corpus, verification, formal, proof, or benchmark source path was modified; `.merge_file_*` debris has been removed.
- [x] Root crate version `0.2.0` and `bamts-cancel` remain present; optional dependency changes are justified by owned source use.
  - Evidence: `git show HEAD:Cargo.toml` and the working tree both list `workspace.package.version = "0.2.0"`, workspace member `crates/bamts-cancel`, and `bamts-cancel.workspace = true` in `[workspace.dependencies]`. `base64 0.23.1`, `unicode-normalization 0.1.25`, and `icu_properties 2.3.0` were not added because no owned root source references them; uses were found only in compiler/CLI and runtime/native surfaces.
- [x] The current benchmark contract binaries are registered in the root `Cargo.toml` and `Cargo.lock`.
  - Evidence: root `Cargo.toml` now contains `[package] name = "bamts-bench"` and `[[bin]]` sections for `jit_benchmarks`, `stage0_evidence`, and `stage1_regression_guard`. `Cargo.lock` was regenerated with `cargo update --workspace` and shows `bamts-bench` plus the resolved dependencies. A second `cargo update --workspace` after `VerificationProofRecovery` added `serde-saphyr = "=1.1.0"` to `crates/bamts-verification/Cargo.toml` locked 0 additional packages, confirming the lock is already current.
- [x] No stale stash deletion, merge debris, conflict marker, stale parallel representation, TODO, placeholder, mock, or no-op remains in ported paths.
  - Evidence: six `.merge_file_*` debris files were removed with `rip -f`; root files were reconciled to HEAD; stale stash deletions of `.gitattributes` and `bamts-cancel` were rejected; no TODO, placeholder, mock, or no-op was introduced.
- [x] No completion gate is marked met from source presence alone.
  - Evidence: the parent integration gates below and the evidence-ledger gate above are explicitly left unchecked.
- [x] Every `PORTED` path completed implementation, expert-reread, defect-hunt, and free-polish inspection passes.
  - Evidence: the five `PORTED` paths (`Cargo.toml`, `Cargo.lock`, this gate, `.outline/gates/clean-port-authority-root.md`, and `.outline/port-audit/authority-root.md`) were authored, read back, and cross-checked against `git status` and the stash diff.
- [x] No compiler/CLI, runtime/native, npm/package, root `package.json`, or root npm lockfile path is changed by this slice.
  - Evidence: the only modified tracked files are `Cargo.toml` and `Cargo.lock`; `package.json` and `package-lock.json` were not touched.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust, Node, formal, and workflow build gates.
- [ ] Corpus, conformance, benchmark, and verification test gates.
- [ ] Project-wide gate checker, completion regeneration, and release evidence.
