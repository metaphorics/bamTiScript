# Gates: F3 integration

Scope: Integrate leaves F3.1, F3.2.

- [ ] G1: Every F3 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: F3 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F3 --aspect contract
  EXPECT: CLUSTER F3 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: F3 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F3 --aspect evidence
  EXPECT: CLUSTER F3 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: F3 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F3 --aspect regression
  EXPECT: CLUSTER F3 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: F3 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate F3
  EXPECT: CLUSTER F3 PASS
  EVIDENCE: pending
