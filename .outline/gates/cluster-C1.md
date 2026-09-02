# Gates: C1 integration

Scope: Integrate leaves C1.1, C1.2, C1.3.

- [ ] G1: Every C1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C1 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C1 --aspect contract
  EXPECT: CLUSTER C1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C1 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C1 --aspect evidence
  EXPECT: CLUSTER C1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C1 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C1 --aspect regression
  EXPECT: CLUSTER C1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C1 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C1
  EXPECT: CLUSTER C1 PASS
  EVIDENCE: pending
