# Gates: B1 integration

Scope: Integrate leaves B1.1, B1.2, B1.3, B1.4.

- [ ] G1: Every B1 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B1 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B1 --aspect contract
  EXPECT: CLUSTER B1 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B1 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B1 --aspect evidence
  EXPECT: CLUSTER B1 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B1 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B1 --aspect regression
  EXPECT: CLUSTER B1 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B1 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B1
  EXPECT: CLUSTER B1 PASS
  EVIDENCE: pending
