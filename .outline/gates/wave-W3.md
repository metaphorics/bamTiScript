# Gates: W3 integration

Scope: Integrate leaves B3.5, B3.6, B3.7, B3.8, B3.9, B4.1, B4.2, B4.3, B4.4, B4.5, C3.1, C3.2, C3.3, C3.4, C3.5, C3.6, C3.7, C4.1, C4.2, C4.3, C5.1, C5.2, C5.3, C5.4, E2.1, E2.2, E2.3.

- [ ] G1: Every W3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W3 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W3 --aspect contract
  EXPECT: WAVE W3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W3 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W3 --aspect evidence
  EXPECT: WAVE W3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W3 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W3 --aspect regression
  EXPECT: WAVE W3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W3 passes its integration gate
  CHECK: just wave-gate W3
  EXPECT: WAVE W3 PASS
  EVIDENCE: pending
