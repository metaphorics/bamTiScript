# Gates: F — Formal correspondence, Node API package, and release

Scope: Integrate clusters F1, F2, F3.

- [ ] G1: Every Track F cluster ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: Track F interfaces and generated artifacts match the frozen contract
  CHECK: cargo run -p bamts-verification -- completion verify --track F --aspect contract
  EXPECT: TRACK F CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: Track F has complete current evidence for every scoped obligation
  CHECK: cargo run -p bamts-verification -- completion verify --track F --aspect evidence
  EXPECT: TRACK F EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: Track F has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --track F --aspect regression
  EXPECT: TRACK F REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: Track F passes its repository integration gate
  CHECK: just track-gate F
  EXPECT: TRACK F PASS
  EVIDENCE: pending
