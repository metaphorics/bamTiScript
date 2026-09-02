# Gates: E5 integration

Scope: Integrate leaves E5.1, E5.2, E5.3, E5.4.

- [ ] G1: Every E5 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: E5 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E5 --aspect contract
  EXPECT: CLUSTER E5 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: E5 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E5 --aspect evidence
  EXPECT: CLUSTER E5 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: E5 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E5 --aspect regression
  EXPECT: CLUSTER E5 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: E5 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate E5
  EXPECT: CLUSTER E5 PASS
  EVIDENCE: pending
