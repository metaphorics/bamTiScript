# Clean port gate: corpus and workflow

## Scope

Owned surfaces: `.github/**`, `corpus/**`, `bench/**`, `vendor/**`, `docs/**`, and repository workflow/verification scripts not under `crates/**`.

## Manual gates

- [x] Every owned stash delta and current untracked addition is classified exactly once in `.outline/port-audit/corpus-workflow.md`.
- [x] Current SHA-pinned workflows, process-group/cancellation semantics, corpus layout, source-size limits, release structure, and TypeScript 7.0.2 authority remain authoritative.
- [x] Unique still-required corpus specifications, benchmark cases, vendor authority metadata, docs, and tool behavior are ported without stale deletion, markers, TODOs, placeholders, or sibling edits.
- [x] No `.references` implementation code is copied.
- [x] Cross-owner registry needs are recorded as `DEFERRED_CROSS_SLICE`.
- [x] No stale stash deletion, merge debris, conflict marker, or stale parallel representation remains in ported paths.

## Parent integration gates

These remain pending for the parent integration pass; this slice does not run them.

- [ ] Formatter and lint gates.
- [ ] Rust and Node build gates.
- [ ] Corpus, benchmark, and workflow test gates.
- [ ] Project-wide gate checker and release evidence.

## Evidence

- `.outline/port-audit/corpus-workflow.md` contains the classified path ledger and cross-slice contracts.
- The seven new local corpus specs are rewritten to the current `RawSpec` schema and the `corpus/manifest.toml` is updated to reference them.
- `.github/workflows/pr.yml` and the three `.merge_file_*` debris files are removed before the integration pass.
