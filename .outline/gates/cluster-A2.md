# Gates: A2 integration

Scope: Integrate leaves A2.1, A2.2, A2.3, A2.6, A2.7.

- [ ] G1: Every A2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: A2 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A2 --aspect contract
  EXPECT: CLUSTER A2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: A2 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A2 --aspect evidence
  EXPECT: CLUSTER A2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: A2 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A2 --aspect regression
  EXPECT: CLUSTER A2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: A2 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate A2
  EXPECT: CLUSTER A2 PASS
  EVIDENCE: pending
