# Gates: E1 integration

Scope: Integrate leaves E1.1, E1.2.

- [ ] G1: Every E1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: E1 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E1 --aspect contract
  EXPECT: CLUSTER E1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: E1 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E1 --aspect evidence
  EXPECT: CLUSTER E1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: E1 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E1 --aspect regression
  EXPECT: CLUSTER E1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: E1 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate E1
  EXPECT: CLUSTER E1 PASS
  EVIDENCE: pending
