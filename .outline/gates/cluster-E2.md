# Gates: E2 integration

Scope: Integrate leaves E2.1, E2.2, E2.3.

- [ ] G1: Every E2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: E2 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E2 --aspect contract
  EXPECT: CLUSTER E2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: E2 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E2 --aspect evidence
  EXPECT: CLUSTER E2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: E2 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E2 --aspect regression
  EXPECT: CLUSTER E2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: E2 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate E2
  EXPECT: CLUSTER E2 PASS
  EVIDENCE: pending
