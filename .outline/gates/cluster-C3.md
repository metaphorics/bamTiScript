# Gates: C3 integration

Scope: Integrate leaves C3.1, C3.2, C3.3, C3.4, C3.5, C3.6, C3.7.

- [ ] G1: Every C3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C3 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C3 --aspect contract
  EXPECT: CLUSTER C3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C3 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C3 --aspect evidence
  EXPECT: CLUSTER C3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C3 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C3 --aspect regression
  EXPECT: CLUSTER C3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C3 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C3
  EXPECT: CLUSTER C3 PASS
  EVIDENCE: pending
