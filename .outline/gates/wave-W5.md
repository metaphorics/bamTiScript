# Gates: W5 integration

Scope: Integrate leaves C10.1, C10.2, C10.3, C10.4, C10.5, C10.6, C11.1, C11.2, C11.3, C11.4, E4.1, E4.2, E4.3, E4.4, E4.5.

- [ ] G1: Every W5 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: W5 child interfaces integrate without an ownership collision
  CHECK: cargo run -p bamts-verification -- completion verify --wave W5 --aspect contract
  EXPECT: WAVE W5 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: W5 receipts form the exact expected closed set
  CHECK: cargo run -p bamts-verification -- completion verify --wave W5 --aspect evidence
  EXPECT: WAVE W5 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: W5 has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --wave W5 --aspect regression
  EXPECT: WAVE W5 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: W5 passes its integration gate
  CHECK: just wave-gate W5
  EXPECT: WAVE W5 PASS
  EVIDENCE: pending
