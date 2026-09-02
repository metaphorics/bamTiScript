# Gates: bamTiScript stable TypeScript 7.0.2 completion

Scope: Complete the clean-room Rust compiler, TypeScript-compatible Node 24 API package, ECMAScript runtime, native tiers, formal proof bindings, target cells, and release evidence for the stable TypeScript 7.0.2 product contract.

- [ ] G1: Every leaf and integration ledger is met with recorded evidence
  CHECK: python3 /home/alpha/.claude/plugins/cache/odin-marketplace/odin/1.17.108/skills/unlazy/scripts/gate_check.py --status .outline/gates/*.md
  EXPECT: ALL MET
  EVIDENCE: 2026-08-28 not run to ALL MET; leaf ledgers still have unchecked G1-G6 boxes (F2.1, A3.1, C4.2, A2.7 G2-G6)

- [ ] G2: The immutable authority and generated catalogs bind stable TypeScript 7.0.2 and no stale primary TypeScript release
  CHECK: cargo run -p bamts-verification -- completion verify --root authority
  EXPECT: AUTHORITY COMPLETE release=typescript-7.0.2
  EVIDENCE: 2026-08-28 `bamts-verification completion verify --root authority` exit 1: unproven children A1.1 through A1.5, A2.1 through A2.3, A2.6, A2.7, and 11 more

- [ ] G3: Every applicable stable TypeScript 7.0.2 compiler, toolchain, CLI, and historical-compatibility obligation has an observed PASS receipt
  CHECK: cargo run -p bamts-verification -- completion verify --root compiler
  EXPECT: COMPILER COMPLETE release=typescript-7.0.2 blocking=0 external_blocked=0
  EVIDENCE: 2026-08-28 `completion verify --root compiler` exit 1: unproven B0.1, B1.1 through B1.4, B2.1, B2.2, B3.1 through B3.3, and 19515 more. Ledger n=78498 PASS=0 BLOCKING_FAIL=71750
  BASELINE: 2026-09-02 `bamts-verification suite run --catalog typescript-7.0.2 --shard i/64` with `BAMTS_SUITE_COMPILER_ADAPTER` set, receipts under `target/evidence-sweep/typescript-7.0.2/`. This is a shard sweep, not the CHECK above. 66786 rows: PASS 8951 (13.4%), BLOCKING_FAIL 51141 (76.6%), INAPPLICABLE_LANGUAGE_SERVICE 6693, TIMEOUT 1. Per-facet PASS: diagnostics 6525/15484, symbols 1846/14621, source-map 8/237, types 440/15918, declaration 61/2215, javascript 71/14096, parse 0/3718, trace 0/487, build-info 0/10. The 2026-08-28 line above reads PASS=0 because no adapter was registered, so every cell recorded `no registered adapter` and nothing was measured. These receipts pre-date 979b78c and 6d1ecc3, so they are a baseline for ranking work, never publishable completion evidence.
  FACETS: 2026-09-02 the per-facet figures above are `PASS/total`, which reads every non-PASS row as outstanding work and is wrong for any facet carrying inapplicable rows. Cross-tabulating the same receipts by facet and state gives the applicable denominator, `PASS + BLOCKING_FAIL`. javascript 71/13960 pass, 13989 blocking. types 440/13950, 13510 blocking, 1967 inapplicable. symbols 1846/13951, 12105 blocking, 670 inapplicable. diagnostics 6525/15182, 8657 blocking, 302 inapplicable. declaration 61/2215, 2154 blocking. trace 0/487, 487 blocking. source-map 8/237, 229 blocking. build-info 0/10, 10 blocking. parse has zero applicable rows: all 3718 are fourslash language-service cases and all are INAPPLICABLE_LANGUAGE_SERVICE, so `parse 0/3718` names an exclusion, never a gap. Pass share of applicable rows is 8951/60092 = 14.9%; the 13.4% above divides by all 66786 rows including exclusions.
  RANKING: 2026-09-02 blocking mass by facet, which is the work order: javascript 13989, types 13510, symbols 12105, diagnostics 8657, declaration 2154, trace 487, source-map 229, build-info 10. Two facets have zero passing applicable rows: trace, whose largest single family is 78 rows of `classification/execution drift: no owned .trace.json baseline` and 31 of `invalid project trace fixture: missing field inputFiles`, and build-info, which has no observer function at all. Classification drift is not systemic: 218 of 51141 blocking rows (0.4%) carry it, over missing `.js` (127), `.trace.json` (78), `.js.map` (9), `.types` (2), and `.symbols` (2) baselines, so the blocking count stands essentially as measured.

- [ ] G4: The bamti Node 24 package implements tsc and all thirteen stable native-preview export subpaths, including API-reachable language-service behavior
  CHECK: cargo run -p bamts-verification -- completion verify --root package
  EXPECT: PACKAGE COMPLETE exports=13 cli=PASS api=PASS
  EVIDENCE: 2026-08-28 `completion verify --root package` exit 1: unproven F2.1 through F2.4, cluster:F2, root:package; F2.1 through F2.3 have no checked mutation contract; `typescript-7.0.2:tests/cases/compiler/2dArrays.ts` BLOCKING_FAIL and 19363 more. F2.1 orphan path is gone

- [ ] G5: The ECMAScript runtime passes every applicable test262 obligation in Interpreter, JIT, and AOT modes
  CHECK: cargo run -p bamts-verification -- completion verify --root runtime
  EXPECT: RUNTIME COMPLETE modes=3 blocking=0 external_blocked=0
  EVIDENCE: 2026-08-28 `completion verify --root runtime` exit 1: unproven C0.1, C1.1 through C1.3, C10.1 through C10.6, and 53679 more

- [ ] G6: Interpreter, JIT, AOT, formal correspondence, target cells, and performance evidence satisfy every compiler-product release obligation
  CHECK: cargo run -p bamts-verification -- completion verify --root native
  EXPECT: NATIVE COMPLETE formal=PASS targets=28/28 benchmarks=9/9
  EVIDENCE: 2026-08-28 `completion verify --root native` exit 1: unproven E1.1, E1.2, E2.1 through E2.3, E3.1 through E3.3, E4.1, E4.2, and 389 more

- [ ] G7: The strict TypeScript-product completion gate accepts only receipt-backed PASS states or exact machine-evidenced policy exclusions
  CHECK: cargo run -p bamts-verification -- completion verify --root product
  EXPECT: PRODUCT COMPLETE release=typescript-7.0.2 blocking=0 external_blocked=0 catalog_errors=0
  EVIDENCE: 2026-08-28 `completion verify --root product` exit 1: unproven B0.1, B1.1 through B3.3, and 73626 more. `ledger rebuild --check` reports E_SET_MISMATCH: no canonical receipt set found
  RECEIPTS: `ts_conformance --shards k/N` is deliberately one-based. `bamts-verification suite run --shard i/N` is deliberately zero-based. Workflows convert once before invoking the low-level runner.

- [ ] G8: Repository-required Rust, formal, corpus, npm, workflow, and release checks pass once on the completed tree
  CHECK: just release-gate
  EXPECT: RELEASE GATE PASS
  EVIDENCE: 2026-08-28 not run; the root `Justfile` now defines the ordered fail-closed release gate

- [x] G9: Regenerating every authority-derived artifact is byte-identical to the committed completion evidence
  CHECK: cargo run -p bamts-verification -- completion regenerate --check
  EXPECT: COMPLETION PROGRAM PASS mode=check
  EVIDENCE: 2026-08-28 `cargo run -p bamts-verification --locked -- completion regenerate --check` → `COMPLETION PROGRAM PASS mode=check` (exit 0). A2.7/A3.1/C4.2/F2.1 `owns` match `.outline/type-script-7.0.2-completion.md` Owned paths (`ci.yml`; `verification/evidence/typescript-7.0.2/*.jsonl`; `regexp_v/`; `api_server/`)
