# Gates: A1 integration

Scope: Integrate leaves A1.1, A1.2, A1.3, A1.4, A1.5.

- [ ] G1: Every A1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: A1 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A1 --aspect contract
  EXPECT: CLUSTER A1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: A1 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A1 --aspect evidence
  EXPECT: CLUSTER A1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: A1 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A1 --aspect regression
  EXPECT: CLUSTER A1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: A1 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate A1
  EXPECT: CLUSTER A1 PASS
  EVIDENCE: pending
