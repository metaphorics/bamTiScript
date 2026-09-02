# Clean port gate: verification and proof

## Scope

Owned surfaces: `crates/bamts-verification/**`, `verification/**`, `proof/**`, `formal/**`, and the durable recovery artifacts `.outline/gates/clean-port-verification-proof.md` and `.outline/port-audit/verification-proof.md`.

## Manual gates

- [ ] Every owned stash delta and current untracked addition is classified exactly once in `.outline/port-audit/verification-proof.md`.
- [ ] Current verification module layout, cancellation/process-group behavior, current manifest, strict receipts, and formal/proof architecture remain authoritative.
- [ ] Root crate version `0.2.0` and `bamts-cancel` remain present; optional dependency changes are justified by owned source use.
- [ ] No stale stash deletion, merge debris, conflict marker, stale parallel representation, TODO, placeholder, mock, or no-op remains in ported paths.
- [ ] No completion gate is marked met from source presence alone.
- [ ] Every `PORTED` path completed implementation, expert-reread, defect-hunt, and free-polish inspection passes.
- [ ] No compiler/CLI, runtime/native, npm/package, root `package.json`, or root npm lockfile path is changed by this slice.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust, Node, formal, and workflow build gates.
- [ ] Corpus, conformance, benchmark, and verification test gates.
- [ ] Project-wide gate checker, completion regeneration, and release evidence.
- [ ] Root `Cargo.lock` regeneration to include `serde-saphyr = "=1.1.0"` and root workspace `[[bin]]`/`autobins` registration for `bench/*.rs` wrappers.

## Evidence

- `.outline/port-audit/verification-proof.md` records 113 owned paths classified as `PORTED` or `ALREADY_HEAD`, with stash deletions ignored and cross-slice contracts deferred to parent integration.
- `crates/bamts-verification/Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/corpus.rs`, `src/ledger.rs`, `src/workspace_guard.rs`, and `tests/corpus_differential.rs` are reconciled from `stash@{0}`; suite-run/merge commands and stale `[[bin]]` entries are removed.
- `crates/bamts-verification/src/corpus.rs` `TASK_107_NODE_CASE_IDS` is extended to the seven new local corpus IDs in manifest order.
