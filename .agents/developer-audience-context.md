# Developer audience context

Last updated: 2026-08-25

## Product overview

| Field | Current position |
| --- | --- |
| Product | bamTiScript |
| One-line description | A pre-release, clean-room Rust implementation of TypeScript 7.0.2 with a `tsc`-compatible command line, type checking, JavaScript execution, native code generation, and formal models. |
| Category | Open-source compiler, command-line tool, runtime, and developer infrastructure |
| Core technology | Rust 2024, TypeScript 7.0.2, Node.js 24 host integration, interpreter, JIT, AOT, Racket models, Lean 4 proofs, and Quint specifications |
| Pricing | MIT-licensed open source |

## Developer persona

The primary reader works on TypeScript tooling, compilers, language runtimes, static analysis, or build infrastructure. They are likely a senior individual contributor, staff engineer, researcher, or tool maintainer. They can read Rust and TypeScript, evaluate compatibility evidence, and build the project from source. The repository does not yet establish a commercial buyer or a specific company-size segment.

## Where they spend time

Likely technical channels include GitHub repositories and issues, Rust compiler communities, TypeScript compiler discussions, programming-language research forums, and systems-programming conferences. The project has not collected channel or community data yet, so no specific forum, newsletter, or event is treated as proven.

## Problems and pain points

- TypeScript users depend on a mature JavaScript implementation whose compiler, runtime, and host behavior are difficult to study or replace as one system.
- Compiler engineers need exact diagnostics, compatibility evidence, and reproducible native execution rather than broad compatibility claims.
- Runtime work spans parsing, checking, JavaScript semantics, module resolution, native lowering, and target behavior. Failures often hide at the boundaries between those parts.

The repository has not collected user interviews or verbatim problem statements yet.

## Current alternatives

- The official TypeScript compiler and language services.
- JavaScript and TypeScript toolchains written in Rust, Go, or JavaScript.
- Separate type checkers, transpilers, bundlers, and JavaScript runtimes.
- Internal compiler or static-analysis infrastructure.

No competitive ranking is established. The official TypeScript 7.0.2 behavior is the compatibility authority for this project.

## Key differentiators

- One Rust workspace owns parsing, checking, emitting, runtime execution, JIT, AOT, and verification.
- Compatibility work binds to a fixed TypeScript release and records machine-readable evidence.
- The project includes formal models and target-cell evidence alongside executable tests.
- The implementation is clean-room work. The `.references/` tree is study material, not source material.

These are architectural facts, not performance or completeness claims. The project remains pre-release.

## Verbatim developer language

No issue, interview, support, or community corpus has been collected. Do not invent quotations. Add exact language here only when a public issue, discussion, or user conversation provides it.

## Technical trust signals

- Public MIT license.
- Rust workspace with compiler, bytecode, runtime, native code generation, CLI, Node host, and verification crates.
- GitHub Actions for pull requests, nightly checks, weekly audits, and releases.
- Test262, TypeScript suite, corpus, target, performance, and formal-evidence machinery in the repository.

Only passing published checks should appear as badges or release claims.

## Conversion actions

1. Read the README and inspect the compatibility scope.
2. Clone the repository and run the two-minute source quick start.
3. Try `bamts` on a real TypeScript file.
4. Read the architecture and verification material before relying on pre-release behavior.
5. Open a focused issue with a reproducible TypeScript case.

## Voice and tone

Write for technical readers. Be direct, specific, and evidence-led. State release status and limits early. Prefer commands, file paths, observed outputs, and exact compatibility targets over promotional language. Do not claim parity, speed, safety, or production readiness without a published receipt.
