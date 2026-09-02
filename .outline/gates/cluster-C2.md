# Gates: C2 integration

Scope: Integrate leaves C2.1, C2.2, C2.3, C2.4.

- [ ] G1: Every C2 leaf ledger is met with evidence
  CHECK: cargo run -p bamts-verification -- gates status
  EXPECT: GATES STATUS PASS
  EVIDENCE: pending

- [ ] G2: C2 child interfaces match the frozen completion-program contract
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C2 --aspect contract
  EXPECT: CLUSTER C2 CONTRACT PASS
  EVIDENCE: pending

- [ ] G3: C2 merged evidence is complete and contains no conflicting ownership
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C2 --aspect evidence
  EXPECT: CLUSTER C2 EVIDENCE PASS
  EVIDENCE: pending

- [ ] G4: C2 integration has no PASS-to-non-PASS transition
  CHECK: cargo run -p bamts-verification -- completion verify --cluster C2 --aspect regression
  EXPECT: CLUSTER C2 REGRESSION PASS
  EVIDENCE: pending

- [ ] G5: C2 passes its narrow integration build, lint, format, and behavioral checks
  CHECK: just cluster-gate C2
  EXPECT: CLUSTER C2 PASS
  EVIDENCE: pending

## Verification 2026-08-27 (post-966 restore)

Cluster cannot close. C2.1 and C2.2 remain unwired (orphan `proxy.rs`/`reflect.rs`,
no `HeapEntry::Proxy`/`ProxyRevoker`). C2.3 and C2.4 are behavior-green (10/10 and
8/8) but have no receipts. G1-G5 stay pending.
