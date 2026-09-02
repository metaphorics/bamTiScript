# Gates: C11 integration

Scope: Integrate leaves C11.1, C11.2, C11.3, C11.4.

- [ ] G1: Every C11 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C11 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C11 --aspect contract
  EXPECT: CLUSTER C11 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C11 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C11 --aspect evidence
  EXPECT: CLUSTER C11 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C11 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C11 --aspect regression
  EXPECT: CLUSTER C11 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C11 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C11
  EXPECT: CLUSTER C11 PASS
  EVIDENCE: pending
