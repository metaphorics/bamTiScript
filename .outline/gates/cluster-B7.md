# Gates: B7 integration

Scope: Integrate leaves B7.1.

- [ ] G1: Every B7 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B7 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B7 --aspect contract
  EXPECT: CLUSTER B7 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B7 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B7 --aspect evidence
  EXPECT: CLUSTER B7 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B7 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B7 --aspect regression
  EXPECT: CLUSTER B7 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B7 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B7
  EXPECT: CLUSTER B7 PASS
  EVIDENCE: pending
