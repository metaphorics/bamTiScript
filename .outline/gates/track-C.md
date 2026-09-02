# Gates: C — ECMAScript runtime semantics required by native execution

Scope: Integrate clusters C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11.

- [ ] G1: Every Track C cluster ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: Track C interfaces and generated artifacts match the frozen contract
  CHECK: cargo run -p bamts-verification -- completion verify --track C --aspect contract
  EXPECT: TRACK C CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: Track C has complete current evidence for every scoped obligation
  CHECK: cargo run -p bamts-verification -- completion verify --track C --aspect evidence
  EXPECT: TRACK C EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: Track C has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --track C --aspect regression
  EXPECT: TRACK C REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: Track C passes its repository integration gate
  CHECK: just track-gate C
  EXPECT: TRACK C PASS
  EVIDENCE: pending
