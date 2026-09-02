# Gates: W0 integration

Scope: Integrate leaves A1.1, A1.2, A1.3, A1.4, A1.5, A2.1, A2.2, A2.3, A2.6, A2.7.

- [ ] G1: Every W0 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W0 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W0 --aspect contract
  EXPECT: WAVE W0 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W0 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W0 --aspect evidence
  EXPECT: WAVE W0 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W0 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W0 --aspect regression
  EXPECT: WAVE W0 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W0 passes its integration gate
  CHECK: just wave-gate W0
  EXPECT: WAVE W0 PASS
  EVIDENCE: pending
