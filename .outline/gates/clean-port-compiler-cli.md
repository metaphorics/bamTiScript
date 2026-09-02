# Clean-port gate: compiler and CLI

## Scope

This gate owns only `crates/bamts-compiler/**`, `crates/bamts-parser/**`, `crates/bamts-project/**`, `crates/bamts-emitter/**`, `crates/bamts-cli/**`, and `.outline/{gates,port-audit}/` artifacts named for this slice. The tracked stash delta is measured from `stash@{0}^1` to `stash@{0}`; current untracked additions are measured from the working tree. Current HEAD remains the architectural base.

## Manual gates

- [x] **M1 — Exhaustive inventory:** `.outline/port-audit/compiler-cli.md` records all 20 owned tracked stash deltas and all 37 owned current untracked additions exactly once.
- [x] **M2 — Stale architecture rejected before editing:** the audit rejects the standalone binder, obsolete overload resolver, stale CLI integration test rewrite, and stale JSX/suspend representations rather than applying old files wholesale.
- [ ] **M3 — Current module graph integrated:** every `PORTED` module is registered through the current compiler, project, emitter, service, or CLI root with no parallel registry.
- [ ] **M4 — Compiler semantics integrated:** parser/scanner residuals, advanced types, decorators, enum/namespace checks, diagnostic mapping, project behavior, emitter behavior, and CLI behavior reach their current callsites.
- [ ] **M5 — Current architecture preserved:** symbol evidence proves cancellation, source-size budgets, JSX AST, `ExecutionContext`, API server, and module organization remain the current HEAD designs.
- [ ] **M6 — Suspend ABI preserved:** every owned producer uses `Suspend { dst, src, resume, mode }`, one successor, and `RESUME_NEXT`/`RESUME_THROW`/`RESUME_RETURN`; no `resume_return` is introduced.
- [ ] **M7 — Four-pass review complete:** every `PORTED` path has implementation, expert-reread, defect-hunt, and free-polish evidence in the audit.
- [ ] **M8 — Ownership/accounting complete:** static path accounting reports no compiler-CLI changes outside this slice and classification totals equal the 57-path inventory.

## Parent integration gates — deliberately pending

The parent owns these checks after all workers settle. This slice does not infer or mark them complete from source inspection.

- [ ] `cargo fmt --check`
- [ ] compiler/CLI build and Clippy checks
- [ ] compiler/parser/checker/emitter/project/CLI behavioral tests
- [ ] workspace tests
- [ ] TypeScript 7.0.2 leaf/cluster receipts for B1.1–B6.2
- [ ] runtime/native differential evidence for compiler-produced helpers
- [ ] package/API end-to-end validation of `bamti --api`
