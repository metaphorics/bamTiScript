# Gates: B0 integration

Scope: Integrate leaves B0.1.

- [ ] G1: Every B0 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B0 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B0 --aspect contract
  EXPECT: CLUSTER B0 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B0 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B0 --aspect evidence
  EXPECT: CLUSTER B0 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B0 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B0 --aspect regression
  EXPECT: CLUSTER B0 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B0 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B0
  EXPECT: CLUSTER B0 PASS
  EVIDENCE: pending
