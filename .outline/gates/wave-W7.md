# Gates: W7 integration

Scope: Integrate leaves F2.1, F2.2, F2.3, F2.4, F3.1, F3.2.

- [ ] G1: Every W7 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W7 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W7 --aspect contract
  EXPECT: WAVE W7 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W7 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W7 --aspect evidence
  EXPECT: WAVE W7 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W7 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W7 --aspect regression
  EXPECT: WAVE W7 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W7 passes its integration gate
  CHECK: just wave-gate W7
  EXPECT: WAVE W7 PASS
  EVIDENCE: pending
