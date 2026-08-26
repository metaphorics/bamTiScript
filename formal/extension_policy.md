# Formal model extension policy

Formal catalog identifiers enter the locked TypeScript 7.0.2 inventory only through
manifest regeneration plus a current G4 authority receipt. No other path may grow,
shrink, or rewrite `formal-lean`, `formal-quint`, or `formal-redex`. This file is
the maintainer contract for F1.2. `formal/ledger_wiring.rs::validate_formal_extension`
is the executable guard.

## 1. Admission source

1. Author the new obligation in its authority file first. G4 later demands that
   the identifier occur exactly once in a declared file.
2. Record the identifier in `verification/catalog-inputs.json` under the matching
   catalog (`formal-quint`, `formal-lean`, or `formal-redex`). Keep that array
   strictly sorted. Do not invent an identifier that the extractor metadata below
   cannot justify.
3. Do not edit `verification/manifest.lock.json`. Regeneration is the only writer.
4. Do not edit `proof/completeness-ledger.json`. Ledger rebuild is the only writer.

`bamts-verification catalog regenerate --release typescript-7.0.2` copies the
formal identifier lists from `verification/catalog-inputs.json` into
`verification/manifest.lock.json`. `--check` proves the lock already matches that
generation. Hand-patching the lock, rewriting `identifiers_sha256`, or splicing a
row into the completeness ledger is a policy violation even when the JSON still
parses.

## 2. G4 authority

After regeneration, G4 must pass on the new lock. G4 is
`bamts-verification formal audit --gates G1,G2,G5,G3,G4,G6` (dependency-closed
`Gate::FORMAL_ORDER`; G4 is not a solo switch). The named G4 artifact is
`verification/manifest.lock.json`.

G4 calls, for every formal identifier:

- `validate_quint_catalog_authority` — the Quint property is declared exactly
  once in the named `.qnt` authority.
- `validate_lean_catalog_authority` — the Lean theorem is declared exactly once
  in the named `.lean` authority.
- `validate_redex_catalog_authority` — each non-control identifier and each
  `control::` test-case is declared exactly once in the named `.rkt` authority.

An identifier that is in the lock but missing, duplicated, or outside
`declared_files` is `E_SET_MISMATCH`. Catalog identifier additions or removals
without a current G4 receipt that covers the regenerated formal `ObligationKey`
set exactly fail `validate_formal_extension` with `E_GATE_DEPENDENCY`. A G4
receipt whose digest is not the current `verification/manifest.lock.json` digest
fails closed (`E_DIGEST`). A G4 receipt that covers any other key set fails
`E_SET_MISMATCH`.

## 3. Catalog-specific derivation

### Quint

Identifiers are `{file}.qnt::{Property}` drawn from the closed-plan inventory in
`verification/catalog-inputs.json`. Every catalog property must appear in
`verification/formal/quint/runs.json` (or its liveness bindings) before G2 can
name it. G2's named proof artifact is that runs file.

### Lean

Direct theorems are `{File}.lean::{theorem}` in declared Lean sources. Derived
simulation lemmas follow the `derived_simulation_lemmas` extractor:

- `source_catalog`: `formal-redex`
- `source_match`: `^ecmascript/(semantics|modules)\.rkt::rule::(?P<rule>.+)$`
- `transform`: `Bamti/Compiler/Correctness.lean::simulate_<rule>`

A new `ecmascript/semantics.rkt::rule::{rule}` or
`ecmascript/modules.rkt::rule::{rule}` identifier requires the matching
`Bamti/Compiler/Correctness.lean::simulate_{rule}` identifier in `formal-lean`
before regenerate. Do not add a `simulate_*` lemma that the transform cannot
produce. Do not omit one that it must produce. G3's named proof artifact is
`proof/lean-assumptions.json`; public theorems there must match the Lean catalog
exactly. Approved kernel assumptions are `Classical.choice`, `Quot.sound`, and
`propext` only. User axioms are forbidden.

### Redex

The control extractor is `relations x non_vacuity_obligations`. G4 reconstructs
the control set as every `control::{relation}::{obligation}` for `relation` in
`relations` and `obligation` in `non_vacuity_obligations`, and demands that set
equal the catalog identifiers that start with `control::`. Direct identifiers
(relations, named `rule::…` labels, declarations) stay outside that product and
must still occur exactly once. `files_without_direct_obligations` may host
syntax; they still cannot hide a catalog identifier. G5's named proof artifact
is the `formal/redex` tree.

## 4. Forbidden shortcuts

G4 `audit_forbidden_material` rejects proof holes in Quint, Lean, Redex, and the
proof control JSON:

- `sorry`, `admit`, `axiom`, `implemented_by`
- `xfail`, `expected-failure`, `expected_failure`
- `negate-invariant`
- `skip` / `#:skip` / `test.skip` / `skip-test`
- Racket `#;`

Do not comment a declaration into existence. Do not xfail a property to mint a
catalog row. An identifier whose only witness is a skipped test is not admitted.

## 5. Ledger PASS

F1.1 projection (`project_formal_ledger`) is the only function that may flip a
P0.7 formal row to `PASS`. The locked inventory is 80 Lean + 49 Quint + 110 Redex
= 239 rows, owner `P0.7`, reason
`{case} has not passed its proof or property gate.` until PASS.

PASS requires all of:

1. a dependency-closed G1–G6 receipt prefix in `Gate::FORMAL_ORDER`;
2. every required named proof receipt for that catalog covering the exact
   `ObligationKey` (catalog, case, `default`, `interpreter`,
   `x86_64-unknown-linux-gnu`);
3. each receipt `evidence_digest` equal to the current named artifact digest.

Named artifacts:

| Gate | Artifact |
| --- | --- |
| G1 | `formal/toolchains.toml` |
| G2 | `verification/formal/quint/runs.json` |
| G3 | `proof/lean-assumptions.json` |
| G4 | `verification/manifest.lock.json` |
| G5 | `formal/redex` |
| G6 | `verification/formal/trace-fixtures.jsonl` |

Required receipts by catalog: Quint `G1,G2,G4,G6`; Lean `G1,G3,G4,G6`; Redex
`G1,G5,G4,G6`. Missing coverage leaves `BLOCKING_FAIL`. A stale digest, unknown
key, wrong-catalog coverage, or non-prefix gate set fails closed and never mints
PASS. Status cannot flip from a gate count alone.

## 6. Ownership

A leaf may change authority sources, `verification/catalog-inputs.json`, this
policy, and `formal/ledger_wiring.rs`. The cluster integrator (Main) is the only
writer of `verification/manifest.lock.json`, `proof/completeness-ledger.json`,
and the one-line `bamts-verification` include:

```rust
#[path = "../../../formal/ledger_wiring.rs"]
pub mod ledger_wiring;
```

Run regenerate, G4, and ledger rebuild through that owner. Do not bypass them
with a direct lock or ledger edit.
