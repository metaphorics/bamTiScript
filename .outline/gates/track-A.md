# Gates: A — Authority, catalogs, receipts, and completion gates

Scope: Integrate clusters A1, A2, A3.

- [ ] G1: Every Track A cluster ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: Track A interfaces and generated artifacts match the frozen contract
  CHECK: cargo run -p bamts-verification -- completion verify --track A --aspect contract
  EXPECT: TRACK A CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: Track A has complete current evidence for every scoped obligation
  CHECK: cargo run -p bamts-verification -- completion verify --track A --aspect evidence
  EXPECT: TRACK A EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: Track A has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --track A --aspect regression
  EXPECT: TRACK A REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: Track A passes its repository integration gate
  CHECK: just track-gate A
  EXPECT: TRACK A PASS
  EVIDENCE: pending
