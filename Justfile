set shell := ["bash", "-euo", "pipefail", "-c"]

# Deterministic receipt-aware TypeScript product release root.
release-gate:
    cargo run --locked -p bamts-verification --bin bamts-verification -- completion regenerate --check
    cargo run --locked -p bamts-verification --bin ts_conformance -- audit-ledger --require-complete
    cargo run --locked -p bamts-verification --bin bamts-verification -- ledger rebuild --check
    cargo run --locked -p bamts-verification --bin bamts-verification -- completion verify --root product
    printf '%s\n' 'RELEASE GATE PASS'
