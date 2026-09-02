# Gates: package completion root

Scope: Track F public package and API contract

- [ ] G1: Every package child ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: The package root has a closed current obligation set
  CHECK: cargo run -p bamts-verification -- completion verify --root package --aspect contract
  EXPECT: ROOT package CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: The package root has no incomplete state
  CHECK: cargo run -p bamts-verification -- completion verify --root package
  EXPECT: PACKAGE COMPLETE exports=13 cli=PASS api=PASS
  EVIDENCE: pending

- [ ] G4: The package root has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --root package --aspect regression
  EXPECT: ROOT package REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: The package root passes its repository gate
  CHECK: just root-gate package
  EXPECT: ROOT package PASS
  EVIDENCE: pending
