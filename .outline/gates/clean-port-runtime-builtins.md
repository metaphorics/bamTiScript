# Clean port gate: runtime builtins

## Scope

Owned surfaces: `crates/bamts-runtime/src/builtins/**` and builtins-only tests/fixtures beneath that tree, plus this slice's two durable audit artifacts.

## Manual gates

- [x] Every owned tracked stash delta and every owned current untracked addition is classified exactly once in `.outline/port-audit/runtime-builtins.md`.
- [x] Stash deletions of the active `date.rs`, `json.rs`, and `uint8array.rs` modules are rejected; current JSON CreateDataProperty and cached typed-array prototype/NewTarget semantics remain authoritative.
- [x] Ported builtins were renamed to current APIs (`EcmaString::encode`, `Machine::to_property_key`, `Machine::has_property`, `Machine::set_data_property_key`) and wired into the active `mod.rs`, `array.rs`, `install_math`, and `collections` (where accepted) with no duplicate representations.
- [x] No stale deletion, TODO, placeholder, no-op, or sibling-owned file edits were introduced.
- [x] Cross-slice candidates (`arraybuffer`, `atomics`, `bigint`, `dataview`, `object_statics`, `property_descriptor`, `proxy`, `reflect`, `regexp_v`, `set_methods`, `string_edge`, `structured_clone`, `typedarray_all`, `weakref_finalization`) are named explicitly in the audit and left untouched; `RuntimeCoreRecovery` was messaged.

## Parent integration gates (pending)

- [ ] Formatter and lint gates.
- [ ] Rust build gates.
- [ ] Runtime/builtins verification tests.
- [ ] Completion leaf and root gate checkers.

## Evidence

- Inventory source: `git diff --name-status --find-renames stash@{0}^1 stash@{0}` restricted to `crates/bamts-runtime/src/builtins`, and `git ls-files -o --exclude-standard -- crates/bamts-runtime/src/builtins`.
- Tracked inventory: 13 `REJECTED_STALE` deltas (active modules preserved).
- Current untracked inventory: 25 paths (7 `PORTED`, 2 `REJECTED`, 16 `DEFERRED_CROSS_SLICE`).
- Total path accounting: 38 paths, each appearing once in `.outline/port-audit/runtime-builtins.md`.
- Current API evidence: `EcmaString::encode` in `crates/bamts-runtime/src/builtins/mod.rs`; `Machine::to_property_key` and `Machine::has_property` in `crates/bamts-runtime/src/lib.rs`; `Machine::set_data_property_key` in `crates/bamts-runtime/src/builtins/mod.rs`.
- Parent validation intentionally pending by assignment; no formatter, linter, build, test, or gate checker was run.
