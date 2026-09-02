# Gates: C5 integration

Scope: Integrate leaves C5.1, C5.2, C5.3, C5.4.

- [ ] G1: Every C5 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C5 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C5 --aspect contract
  EXPECT: CLUSTER C5 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C5 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C5 --aspect evidence
  EXPECT: CLUSTER C5 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C5 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C5 --aspect regression
  EXPECT: CLUSTER C5 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C5 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C5
  EXPECT: CLUSTER C5 PASS
  EVIDENCE: pending
