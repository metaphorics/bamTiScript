# Clean-port gate: compiler frontend

## Scope

This gate owns `crates/bamts-parser/**` (none present), `crates/bamts-compiler/**` scanner/parser/syntax/parse-diagnostic/directive files and dedicated subdirectories/tests, and all residual `crates/bamts-compiler/**` paths not explicitly owned by `CompilerSemanticsRecovery` or `EmitterProjectCliRecovery`, including the compiler crate roots, module registries, and manifests.

It also owns `.outline/gates/clean-port-compiler-cli.md` and `.outline/port-audit/compiler-frontend.md`.

## Manual gates

- [x] **M1 — Exhaustive inventory:** `.outline/port-audit/compiler-frontend.md` records all 30 owned tracked stash deltas and all 36 owned current working-tree additions/modifications exactly once.
- [x] **M2 — Stale architecture rejected before editing:** the audit rejects the standalone binder, obsolete overload resolver, stale `Cargo.toml`/RULES.md churn, and stale JSX/suspend representations rather than applying old files wholesale.
- [x] **M3 — Current module graph integrated:** `diagnostics_parser`, `tsc_directives`, `binder`, `public_ast`, and `service` are registered through `crates/bamts-compiler/src/lib.rs`; `source.rs` exports `JsxEmit` and `NodeIdSource` needed by the new directive, JSX-desugar, emitter, and project consumers.
- [x] **M4 — Frontend behavior complete:** parse-diagnostic parity mapper, tsc harness directive parser, top-level binder export/merge tables, public AST projection, and language-service surface are available.
- [x] **M5 — Current architecture preserved:** cancellation, `MAX_SOURCE_BYTES`, `SourceTooLarge`, JSX AST, `ExecutionContext`/API server, and module organization remain the current HEAD designs.
- [x] **M6 — Suspend ABI preserved:** no `resume_return` or parallel Suspend ABI changes introduced.
- [x] **M7 — Four-pass review complete:** implementation, expert-reread, defect-hunt, and free-polish evidence are recorded in `.outline/port-audit/compiler-frontend.md`.
- [x] **M8 — Ownership/accounting complete:** static path accounting reports no compiler-frontend changes outside this slice; classification totals equal the 64 unique-path inventory (30 stash + 36 working tree minus the `lib.rs`/`source.rs` overlap, `crates/bamts-parser` 0).

## Parent integration gates — deliberately pending

- [ ] `cargo fmt --check`
- [ ] compiler/CLI build and Clippy checks
- [ ] compiler/parser/checker/emitter/project/CLI behavioral tests
- [ ] workspace tests
