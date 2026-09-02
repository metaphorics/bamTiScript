# Gates: E — Bytecode, JIT, AOT, target cells, and performance

Scope: Integrate clusters E1, E2, E3, E4, E5.

- [ ] G1: Every Track E cluster ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: Track E interfaces and generated artifacts match the frozen contract
  CHECK: cargo run -p bamts-verification -- completion verify --track E --aspect contract
  EXPECT: TRACK E CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: Track E has complete current evidence for every scoped obligation
  CHECK: cargo run -p bamts-verification -- completion verify --track E --aspect evidence
  EXPECT: TRACK E EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: Track E has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --track E --aspect regression
  EXPECT: TRACK E REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: Track E passes its repository integration gate
  CHECK: just track-gate E
  EXPECT: TRACK E PASS
  EVIDENCE: pending
