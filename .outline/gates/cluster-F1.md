# Gates: F1 integration

Scope: Integrate leaves F1.1, F1.2.

- [ ] G1: Every F1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: F1 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F1 --aspect contract
  EXPECT: CLUSTER F1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: F1 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F1 --aspect evidence
  EXPECT: CLUSTER F1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: F1 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F1 --aspect regression
  EXPECT: CLUSTER F1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: F1 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate F1
  EXPECT: CLUSTER F1 PASS
  EVIDENCE: pending
