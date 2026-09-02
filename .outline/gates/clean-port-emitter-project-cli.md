# Clean-port gate: emitter, project, and CLI

## Scope

This gate owns the compiler emitter/project files and subdirectories under `crates/bamts-compiler/**`, all of `crates/bamts-cli/**` (when such paths exist), and the `.outline/{gates,port-audit}/` artifacts named for this slice. `crates/bamts-emitter/**` and `crates/bamts-project/**` have no files in the checkout; their behavior is mapped into `crates/bamts-compiler/src/emitter/` and `crates/bamts-compiler/src/project/`.

## Manual gates

- [x] **M1 — Exhaustive inventory:** `.outline/port-audit/emitter-project-cli.md` records all 11 owned tracked stash deltas, all 14 owned current untracked additions, 3 already-HEAD reused paths, and 2 rejected stale paths (30 total).
- [x] **M2 — Stale architecture rejected before editing:** `crates/bamts-cli/src/cli.rs` and `crates/bamts-cli/src/context.rs` are classified `REJECTED_STALE` and are no longer loaded by the new `crates/bamts-cli/src/lib.rs`.
- [x] **M3 — Current module graph integrated:** `crates/bamts-compiler/src/emitter.rs` declares its child modules; `crates/bamts-compiler/src/project.rs` declares its child modules; `crates/bamts-cli/src/lib.rs` exposes `api_server` and the inline `cli { diagnostic_format, tsc_args }` module.
- [x] **M4 — Emitter/project/CLI behavior integrated:** `emitter.rs`, `project.rs`, `pipeline.rs`, `program.rs`, `source.rs`, and `script.rs` restored; `tsc_args.rs`, `diagnostic_format.rs`, and `api_server.rs` wired through `main.rs` dispatch.
- [x] **M5 — Current architecture preserved:** `api_server.rs` uses `bamts_compiler::service::r#async::CancellationToken`; no `resume_return` API; no `TODO:`, `FIXME:`, `unimplemented!()`, or `todo!()` markers; `panic!()` calls are invariant guards or test harness code.
- [x] **M6 — Suspend ABI preserved:** This slice touches no lowerer/bytecode files; the current `Suspend { dst, src, resume, mode }` ABI is unchanged.
- [x] **M7 — Four-pass review complete:** implementation, expert re-read, defect-hunt, and polish passes recorded in `.outline/port-audit/emitter-project-cli.md`.
- [x] **M8 — Ownership/accounting complete:** measured totals (11 stash + 14 untracked + 3 already-HEAD + 2 rejected = 30) and exact paths recorded; no changes outside the owned scope except the registry need forwarded for `public_ast`/`service`.

## Registry need forwarded

`CompilerFrontendRecovery` must add `pub mod public_ast;` and `pub mod service;` to `crates/bamts-compiler/src/lib.rs` so `bamts-cli` `api_server.rs` can resolve `bamts_compiler::public_ast` and `bamts_compiler::service`.

## Parent integration gates — deliberately pending

The parent owns these checks after all workers settle; this slice does not run them.

- [ ] `cargo fmt --check`
- [ ] compiler/CLI build and Clippy checks
- [ ] compiler/CLI behavioral tests
- [ ] workspace tests
