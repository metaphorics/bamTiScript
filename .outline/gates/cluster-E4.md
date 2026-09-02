# Gates: E4 integration

Scope: Integrate leaves E4.1, E4.2, E4.3, E4.4, E4.5.

- [ ] G1: Every E4 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: E4 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E4 --aspect contract
  EXPECT: CLUSTER E4 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: E4 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E4 --aspect evidence
  EXPECT: CLUSTER E4 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: E4 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster E4 --aspect regression
  EXPECT: CLUSTER E4 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: E4 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate E4
  EXPECT: CLUSTER E4 PASS
  EVIDENCE: pending
