# Gates: B4 integration

Scope: Integrate leaves B4.1, B4.2, B4.3, B4.4, B4.5.

- [ ] G1: Every B4 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B4 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B4 --aspect contract
  EXPECT: CLUSTER B4 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B4 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B4 --aspect evidence
  EXPECT: CLUSTER B4 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B4 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B4 --aspect regression
  EXPECT: CLUSTER B4 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B4 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B4
  EXPECT: CLUSTER B4 PASS
  EVIDENCE: pending
