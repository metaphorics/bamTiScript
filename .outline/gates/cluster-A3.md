# Gates: A3 integration

Scope: Integrate leaves A3.1.

- [ ] G1: Every A3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: A3 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A3 --aspect contract
  EXPECT: CLUSTER A3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: A3 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A3 --aspect evidence
  EXPECT: CLUSTER A3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: A3 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster A3 --aspect regression
  EXPECT: CLUSTER A3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: A3 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate A3
  EXPECT: CLUSTER A3 PASS
  EVIDENCE: pending
