# Gates: W2 integration

Scope: Integrate leaves B0.1, B1.1, B1.2, B1.3, B1.4, B2.1, B2.2, B3.1, B3.2, B3.3, B3.4, C0.1, C1.1, C1.2, C1.3, C2.1, C2.2, C2.3, C2.4, E1.1, E1.2.

- [ ] G1: Every W2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W2 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W2 --aspect contract
  EXPECT: WAVE W2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W2 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W2 --aspect evidence
  EXPECT: WAVE W2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W2 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W2 --aspect regression
  EXPECT: WAVE W2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W2 passes its integration gate
  CHECK: just wave-gate W2
  EXPECT: WAVE W2 PASS
  EVIDENCE: pending
