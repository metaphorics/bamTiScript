# Gates: E3 integration

Scope: Integrate leaves E3.1, E3.2, E3.3.

- [ ] G1: Every E3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: E3 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E3 --aspect contract
  EXPECT: CLUSTER E3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: E3 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E3 --aspect evidence
  EXPECT: CLUSTER E3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: E3 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E3 --aspect regression
  EXPECT: CLUSTER E3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: E3 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate E3
  EXPECT: CLUSTER E3 PASS
  EVIDENCE: pending
