# Gates: product TypeScript 7.0.2 completion root

Scope: Integrate compiler, runtime, native, and package roots; full Node reimplementation, N-API, WebAssembly, and WASI are outside this root.

- [ ] G1: Every TypeScript-product child root is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: The product root uses only stable TypeScript 7.0.2 authority and exact policy exclusions
  CHECK: cargo run -p bamts-verification -- completion verify --root product --aspect contract
  EXPECT: ROOT product CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: The product root has zero blocking, external-blocked, catalog-error, timeout, crash, skip, or missing-receipt states
  CHECK: cargo run -p bamts-verification -- completion verify --root product
  EXPECT: PRODUCT COMPLETE release=typescript-7.0.2 blocking=0 external_blocked=0 catalog_errors=0
  EVIDENCE: pending

- [ ] G4: The product root regeneration is byte-identical
  CHECK: cargo run -p bamts-verification -- completion regenerate --check
  EXPECT: REGENERATION IDENTICAL
  EVIDENCE: pending

- [ ] G5: The completed TypeScript product passes the release gate
  CHECK: just release-gate
  EXPECT: RELEASE GATE PASS
  EVIDENCE: pending
