# Gates: C9 integration

Scope: Integrate leaves C9.

- [ ] G1: Every C9 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C9 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C9 --aspect contract
  EXPECT: CLUSTER C9 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C9 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C9 --aspect evidence
  EXPECT: CLUSTER C9 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C9 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C9 --aspect regression
  EXPECT: CLUSTER C9 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C9 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C9
  EXPECT: CLUSTER C9 PASS
  EVIDENCE: pending
