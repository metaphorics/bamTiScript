# Gates: C10 integration

Scope: Integrate leaves C10.1, C10.2, C10.3, C10.4, C10.5, C10.6.

- [ ] G1: Every C10 leaf ledger is met with evidence
  CHECK: cargo run --locked -p bamts-verification --bin bamts-verification -- completion verify --cluster C10 --aspect contract
  EXPECT: CLUSTER C10 CONTRACT PASS
  EVIDENCE: pending

- [ ] G2: C10 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C10 --aspect contract
  EXPECT: CLUSTER C10 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C10 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C10 --aspect evidence
  EXPECT: CLUSTER C10 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C10 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C10 --aspect regression
  EXPECT: CLUSTER C10 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C10 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C10
  EXPECT: CLUSTER C10 PASS
  EVIDENCE: pending
