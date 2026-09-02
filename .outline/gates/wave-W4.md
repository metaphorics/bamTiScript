# Gates: W4 integration

Scope: Integrate leaves B5.1, B5.2, B5.3, B5.4, B6.1, B6.2, B7.1, C6.1, C6.2, C6.3, C7.1, C7.2, C8.1, C8.2, C8.3, C9, E3.1, E3.2, E3.3.

- [ ] G1: Every W4 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W4 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W4 --aspect contract
  EXPECT: WAVE W4 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W4 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W4 --aspect evidence
  EXPECT: WAVE W4 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W4 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W4 --aspect regression
  EXPECT: WAVE W4 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W4 passes its integration gate
  CHECK: just wave-gate W4
  EXPECT: WAVE W4 PASS
  EVIDENCE: pending
