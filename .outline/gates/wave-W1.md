# Gates: W1 integration

Scope: Integrate leaves A3.1.

- [ ] G1: Every W1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W1 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W1 --aspect contract
  EXPECT: WAVE W1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W1 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W1 --aspect evidence
  EXPECT: WAVE W1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W1 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W1 --aspect regression
  EXPECT: WAVE W1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W1 passes its integration gate
  CHECK: just wave-gate W1
  EXPECT: WAVE W1 PASS
  EVIDENCE: pending
