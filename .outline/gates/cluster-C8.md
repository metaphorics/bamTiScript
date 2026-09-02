# Gates: C8 integration

Scope: Integrate leaves C8.1, C8.2, C8.3.

- [ ] G1: Every C8 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C8 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C8 --aspect contract
  EXPECT: CLUSTER C8 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C8 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C8 --aspect evidence
  EXPECT: CLUSTER C8 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C8 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C8 --aspect regression
  EXPECT: CLUSTER C8 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C8 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C8
  EXPECT: CLUSTER C8 PASS
  EVIDENCE: pending
