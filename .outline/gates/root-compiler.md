# Gates: compiler completion root

Scope: Track B compiler semantics and the compiler-facing part of Track F

- [ ] G1: Every compiler child ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: The compiler root has a closed current obligation set
  CHECK: cargo run -p bamts-verification -- completion verify --root compiler --aspect contract
  EXPECT: ROOT compiler CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: The compiler root has no incomplete state
  CHECK: cargo run -p bamts-verification -- completion verify --root compiler
  EXPECT: COMPILER COMPLETE release=typescript-7.0.2 blocking=0 external_blocked=0
  EVIDENCE: pending

- [ ] G4: The compiler root has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --root compiler --aspect regression
  EXPECT: ROOT compiler REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: The compiler root passes its repository gate
  CHECK: just root-gate compiler
  EXPECT: ROOT compiler PASS
  EVIDENCE: pending
