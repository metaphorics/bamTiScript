# Gates: C7 integration

Scope: Integrate leaves C7.1, C7.2.

- [ ] G1: Every C7 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C7 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C7 --aspect contract
  EXPECT: CLUSTER C7 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C7 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C7 --aspect evidence
  EXPECT: CLUSTER C7 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C7 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C7 --aspect regression
  EXPECT: CLUSTER C7 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C7 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C7
  EXPECT: CLUSTER C7 PASS
  EVIDENCE: pending
