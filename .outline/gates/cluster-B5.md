# Gates: B5 integration

Scope: Integrate leaves B5.1, B5.2, B5.3, B5.4.

- [ ] G1: Every B5 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: B5 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B5 --aspect contract
  EXPECT: CLUSTER B5 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: B5 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B5 --aspect evidence
  EXPECT: CLUSTER B5 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: B5 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster B5 --aspect regression
  EXPECT: CLUSTER B5 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: B5 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate B5
  EXPECT: CLUSTER B5 PASS
  EVIDENCE: pending
