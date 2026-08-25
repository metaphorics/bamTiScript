# Contributing to bamTiScript

bamTiScript targets the behavior of stable TypeScript 7.0.2. Keep changes narrow, reproducible, and tied to an observable compiler or runtime contract.

## Before you change code

1. Search existing issues for the same diagnostic, syntax form, runtime behavior, or API surface.
2. Reduce the case to the smallest TypeScript program that still fails.
3. Record the exact command, output, and TypeScript 7.0.2 result.
4. Keep `.references/` read-only. It is study material, not implementation source.

Open an issue before work that changes a public API, diagnostic contract, bytecode format, runtime semantics, evidence schema, or release gate.

## Build and test

The workspace requires Rust 1.97.1 and uses Rust 2024.

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Run the narrow test that covers your change while developing. Run the full commands above once before you submit it. Do not pipe verification commands through output filters because that can hide a failing exit status.

Changes to generated catalogs, evidence, formal bindings, targets, or completion records must also pass the commands in the corresponding GitHub workflow. The workflow files are the command authority:

- [Pull request checks](.github/workflows/pr.yml)
- [Nightly checks](.github/workflows/nightly.yml)
- [Weekly audit](.github/workflows/weekly-audit.yml)
- [Release checks](.github/workflows/release.yml)

## Tests

Add a test only when it protects observable behavior or a boundary that static checks cannot prove. A regression test must fail when the defect returns. Prefer the nearest existing test module or suite. Do not add source-text assertions, duplicate constructor-shape tests, or mocks that replace the behavior under test.

For compatibility defects, include:

- the minimal TypeScript or JavaScript input;
- the bamTiScript command and output;
- the TypeScript 7.0.2 command and output;
- the execution mode when runtime behavior differs between interpreter, JIT, or AOT;
- the host and target when native behavior is target-specific.

## Code standards

- Keep one concern per commit.
- Use the existing module boundary instead of adding a second convention.
- Remove obsolete callers and paths in the same change. Do not add compatibility aliases.
- Keep control flow shallow and comments focused on why a constraint exists.
- Use typed errors at recoverable boundaries. Fail fast on impossible internal states.
- Do not add an unsafe block. The workspace forbids unsafe Rust.

Use conventional commit subjects such as `fix(runtime): preserve signed zero in typed array sort`.

## Pull requests

Describe the defect or requirement, the concrete change, and the verification you ran. Call out known limits. Do not claim complete parity, performance, or target support beyond the evidence included in the repository.

By contributing, you agree that your contribution is licensed under the repository's [MIT License](LICENSE).
