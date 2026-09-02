# Gates: clean-port integration

Scope: Integrate the clean-ported compiler, runtime, verification, and package surfaces on the current remote-main architecture.

Campaign status (2026-08-27): **IN PROGRESS — not ALL MET.** Prior `[x]` marks on this file were a stale integrated snapshot. The current working tree does not compile `bamts-runtime` lib tests (52 errors) and compiler/CLI recovery is in flight. Re-run every CHECK on a clean tree before closing II.0.11. Narrative ledger: `.outline/sdd/reports/integration-ledger.md`.

- [ ] G1: The working tree contains no unmerged paths or merge markers
  CHECK: git diff --name-only --diff-filter=U
  EXPECT: empty
  EVIDENCE: pending

- [ ] G2: The compiler library type-checks with the locked dependency graph
  CHECK: cargo check -p bamts-compiler --lib --locked
  EXPECT: Finished
  EVIDENCE: pending

- [ ] G3: Every workspace target builds with the locked dependency graph
  CHECK: cargo build --workspace --all-targets --locked
  EXPECT: Finished
  EVIDENCE: pending

- [ ] G4: Repository-required tests pass on the integrated tree
  CHECK: cargo test --workspace --locked
  EXPECT: test result: ok
  EVIDENCE: pending

- [ ] G5: Rust formatting and lint checks accept the integrated tree
  CHECK: cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
  EXPECT: Finished
  EVIDENCE: pending

