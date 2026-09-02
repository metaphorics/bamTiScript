# Clean port gate: npm API

## Scope

Owned surfaces: `npm/**`, `packages/**`, `crates/bamts-node/**`, `crates/bamts-napi/**`, root `package.json`, root npm lockfiles, and package/API scripts serving only these surfaces.

## Manual gates

- [x] Every owned stash delta and current untracked addition is classified exactly once in `.outline/port-audit/npm-api.md`.
  - Evidence: 31 unique ledger rows; 29 `PORTED`, 1 `REJECTED_STALE`, 1 `DEFERRED_CROSS_SLICE`, and zero rows in the other two classifications.
- [x] The current direct native loader remains the sole native-addon loader.
  - Evidence: package root `index.js` imports `runNative`; `native-runner.js` is the only package execution module importing `loadNativeAddon`; no ported service or AST module imports the native loader. The persistent service uses `bamti-cli --api` and is not a native loader or fallback.
- [x] The Node 24 package root and all thirteen required export-map entries resolve to current owned files and declarations where applicable.
  - Evidence: `npm/bamti/package.json` requires Node `>=24`; its `exports` object has exactly 13 keys. Mechanical path inspection resolved every `types` and `import` target, plus `./package.json`, to an existing owned file. All 28 service/AST method strings emitted by those modules have matching methods in the current API dispatcher source.
- [ ] Persistent service exports are executable through registered `bamti-cli --api` dispatch.
  - Pending sibling evidence: `api_server.rs` exists and implements the 28 methods, but final inspection found no `pub mod api_server` or `api_server::maybe_run` call in the CLI library/binary. The compiler/CLI owner confirmed this registration is its responsibility.
- [x] Cancellation and native/service errors propagate without fallback or suppression.
  - Evidence: native `run` directly returns `runNative`; Session forwards request options to Transport; Transport rejects pre-aborted and in-flight requests with `AbortError`, forwards `$/cancelRequest`, rejects child crashes/protocol failures/service failures with typed errors, and rejects pending work during disposal. Only best-effort cancellation notification and process-kill failures are intentionally suppressed after the caller already has its terminal error.
- [x] No stale parallel package topology, unresolved owned imports, TODOs, placeholders, mocks, or no-ops remain in ported paths.
  - Evidence: all relative JS/declaration imports under `npm/bamti` resolve; the 0.1.0 package manifest and CLI-backed root `run` were not replayed; `packages/**` was not introduced; source inspection found no implementation markers. The README's explicit 0.1.0 publication status is retained factual release history, not implementation topology.
- [x] Every `PORTED` path completed implementation, expert-reread, defect-hunt, and free-polish inspection passes.
  - Evidence: the four passes and path-specific outcomes are recorded in `.outline/port-audit/npm-api.md`. Final polish added strict byte-limit validation, fatal UTF-8 framing, immediate serialized-service disposal, current artifact topology, and a real service/cancellation clean-install scenario.

## Parent integration gates

These remain pending for the parent integration pass; this slice was explicitly prohibited from running them.

- [ ] Formatter and lint gates.
- [ ] Rust and Node build gates.
- [ ] Package/API tests and clean-install checks.
- [ ] Project-wide gate checker and release evidence.

## Cross-slice integration

The compiler/CLI owner confirmed that `crates/bamts-cli/src/api_server.rs` is the stable persistent transport and is porting its `--api` registration. This npm slice neither edited nor duplicated that sibling implementation. Parent integration must include that registration before executing the pending package tests.
