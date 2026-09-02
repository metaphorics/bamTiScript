# Gates: B2 integration

Scope: Integrate leaves B2.1, B2.2.

- [ ] G1: Every B2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B2 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B2 --aspect contract
  EXPECT: CLUSTER B2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B2 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B2 --aspect evidence
  EXPECT: CLUSTER B2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B2 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B2 --aspect regression
  EXPECT: CLUSTER B2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B2 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B2
  EXPECT: CLUSTER B2 PASS
  EVIDENCE: pending
