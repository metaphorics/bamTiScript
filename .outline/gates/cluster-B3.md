# Gates: B3 integration

Scope: Integrate leaves B3.1, B3.2, B3.3, B3.4, B3.5, B3.6, B3.7, B3.8, B3.9.

- [ ] G1: Every B3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B3 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B3 --aspect contract
  EXPECT: CLUSTER B3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B3 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B3 --aspect evidence
  EXPECT: CLUSTER B3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B3 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B3 --aspect regression
  EXPECT: CLUSTER B3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B3 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B3
  EXPECT: CLUSTER B3 PASS
  EVIDENCE: pending
