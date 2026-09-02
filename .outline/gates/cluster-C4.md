# Gates: C4 integration

Scope: Integrate leaves C4.1, C4.2, C4.3.

- [ ] G1: Every C4 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C4 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C4 --aspect contract
  EXPECT: CLUSTER C4 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C4 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C4 --aspect evidence
  EXPECT: CLUSTER C4 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C4 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C4 --aspect regression
  EXPECT: CLUSTER C4 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C4 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C4
  EXPECT: CLUSTER C4 PASS
  EVIDENCE: pending
