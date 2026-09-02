# Gates: W6 integration

Scope: Integrate leaves E5.1, E5.2, E5.3, E5.4, F1.1, F1.2.

- [ ] G1: Every W6 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W6 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W6 --aspect contract
  EXPECT: WAVE W6 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W6 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W6 --aspect evidence
  EXPECT: WAVE W6 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W6 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W6 --aspect regression
  EXPECT: WAVE W6 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W6 passes its integration gate
  CHECK: just wave-gate W6
  EXPECT: WAVE W6 PASS
  EVIDENCE: pending
