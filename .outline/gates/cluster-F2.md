# Gates: F2 integration

Scope: Integrate leaves F2.1, F2.2, F2.3, F2.4.

- [ ] G1: Every F2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: F2 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F2 --aspect contract
  EXPECT: CLUSTER F2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: F2 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F2 --aspect evidence
  EXPECT: CLUSTER F2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: F2 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster F2 --aspect regression
  EXPECT: CLUSTER F2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: F2 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate F2
  EXPECT: CLUSTER F2 PASS
  EVIDENCE: pending
