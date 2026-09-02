# Gates: C0 integration

Scope: Integrate leaves C0.1.

- [ ] G1: Every C0 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C0 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C0 --aspect contract
  EXPECT: CLUSTER C0 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C0 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C0 --aspect evidence
  EXPECT: CLUSTER C0 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C0 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C0 --aspect regression
  EXPECT: CLUSTER C0 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C0 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C0
  EXPECT: CLUSTER C0 PASS
  EVIDENCE: pending
