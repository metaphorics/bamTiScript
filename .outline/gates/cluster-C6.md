# Gates: C6 integration

Scope: Integrate leaves C6.1, C6.2, C6.3.

- [ ] G1: Every C6 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C6 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C6 --aspect contract
  EXPECT: CLUSTER C6 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C6 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C6 --aspect evidence
  EXPECT: CLUSTER C6 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C6 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C6 --aspect regression
  EXPECT: CLUSTER C6 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C6 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C6
  EXPECT: CLUSTER C6 PASS
  EVIDENCE: pending
