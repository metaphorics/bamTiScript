# Gates: B6 integration

Scope: Integrate leaves B6.1, B6.2.

- [ ] G1: Every B6 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B6 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B6 --aspect contract
  EXPECT: CLUSTER B6 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B6 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B6 --aspect evidence
  EXPECT: CLUSTER B6 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B6 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B6 --aspect regression
  EXPECT: CLUSTER B6 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B6 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B6
  EXPECT: CLUSTER B6 PASS
  EVIDENCE: pending
