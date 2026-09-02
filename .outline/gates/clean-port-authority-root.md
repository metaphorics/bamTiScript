# Clean port gate: root authority

## Scope

Root Rust, toolchain, process documentation, and residual top-level configuration owned by `RootAuthorityRecovery`. Excluded: `.github/**`, `bench/**`, `corpus/**`, `formal/**`, `proof/**`, `verification/**`, compiler/CLI, runtime/native, npm/API, root `package.json`, and root `package-lock.json`.

## Manual gates

- [x] Every owned root stash delta and current untracked addition is classified exactly once in `.outline/port-audit/authority-root.md`.
  - Evidence: 18 paths classified as `ALREADY_HEAD` (7), `REJECTED_INVALID` (6), `PORTED` (5), `DEFERRED_CROSS_SLICE` (0).
- [x] `Cargo.toml` remains at workspace version `0.2.0` with `crates/bamts-cancel` as a workspace member and `bamts-cancel.workspace = true` in `[workspace.dependencies]`.
  - Evidence: `git show HEAD:Cargo.toml` and working-tree `Cargo.toml` both contain `version = "0.2.0"` and the `bamts-cancel` member/dependency.
- [x] `rust-toolchain.toml` remains `1.97.1` with `rustfmt`, `clippy`, and `rust-src`.
  - Evidence: `git show HEAD:rust-toolchain.toml` and working-tree file match.
- [x] No `base64 0.23.1`, `unicode-normalization 0.1.25`, or `icu_properties 2.3.0` dependency is added because no owned root source uses them.
  - Evidence: dependency-use grep found the crates only in compiler/CLI and runtime/native source files; root `Cargo.toml` still omits them.
- [x] `Cargo.toml` registers the three binaries the current benchmark contract requires.
  - Evidence: root `Cargo.toml` now has `[package] name = "bamts-bench"` and `[[bin]]` sections for `jit_benchmarks` (`bench/jit_benchmarks.rs`), `stage0_evidence` (`bench/stage0_evidence.rs`), and `stage1_regression_guard` (`bench/stage1_regression_guard.rs`). `bench/compiler-rules.toml` is not registered because it is a data file, not a binary.
- [x] `Cargo.lock` was regenerated because the root Rust manifest changed.
  - Evidence: `cargo update --workspace` added `bamts-bench` and newly resolved dependencies. A second `cargo update --workspace` after `VerificationProofRecovery` added `serde-saphyr = "=1.1.0"` to `crates/bamts-verification/Cargo.toml` locked 0 additional packages, confirming the lock is current. `git diff --stat -- Cargo.lock` reports +74 / -1 lines.
- [x] No `.merge_file_*` debris remains in the repository root.
  - Evidence: `glob(".merge_file_*")` and `git status` report zero merge-file paths after `rip -f .merge_file_*`.
- [x] No stale stash deletion, conflict marker, stale parallel representation, TODO, placeholder, mock, or no-op remains in root owned paths.
  - Evidence: all root files reconciled to HEAD except the intended `Cargo.toml`/`Cargo.lock` bench registration; stale stash deletions of `.gitattributes` and `bamts-cancel` were rejected.
- [x] No completion gate is marked met from source presence alone.
  - Evidence: parent integration gates below are explicitly left unchecked.
- [x] Every `PORTED` path completed implementation, expert-reread, defect-hunt, and free-polish inspection passes.
  - Evidence: the five `PORTED` paths (`Cargo.toml`, `Cargo.lock`, this gate, `.outline/gates/clean-port-authority-evidence.md`, and `.outline/port-audit/authority-root.md`) were authored, read back, and cross-checked against `git status` and the stash diff.
- [x] No compiler/CLI, runtime/native, npm/package, root `package.json`, or root npm lockfile path is changed by this slice.
  - Evidence: the only modified tracked files are `Cargo.toml` and `Cargo.lock`; `package.json` and `package-lock.json` were not touched.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust, Node, formal, and workflow build gates.
- [ ] Corpus, conformance, benchmark, and verification test gates.
- [ ] Project-wide gate checker, completion regeneration, and release evidence.
