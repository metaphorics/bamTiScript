# Gates: runtime completion root

Scope: Track C ECMAScript behavior in Interpreter, JIT, and AOT

- [ ] G1: Every runtime child ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: The runtime root has a closed current obligation set
  CHECK: cargo run -p bamts-verification -- completion verify --root runtime --aspect contract
  EXPECT: ROOT runtime CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: The runtime root has no incomplete state
  CHECK: cargo run -p bamts-verification -- completion verify --root runtime
  EXPECT: RUNTIME COMPLETE modes=3 blocking=0 external_blocked=0
  EVIDENCE: pending

- [ ] G4: The runtime root has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --root runtime --aspect regression
  EXPECT: ROOT runtime REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: The runtime root passes its repository gate
  CHECK: just root-gate runtime
  EXPECT: ROOT runtime PASS
  EVIDENCE: pending
