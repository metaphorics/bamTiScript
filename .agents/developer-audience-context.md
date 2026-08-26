# Developer Audience Context

Last updated: 2026-08-11

## Product Overview

| Field | Repository Evidence & State |
|-------|-----------------------------|
| **Product Name** | `bamTiScript` (Rust workspace); CLI binary `bamts`; Node/npm shims `bamti` and `bamti-cli`. Repository: `metaphorics/bamTiScript`. |
| **One-liner** | A pre-release TypeScript compiler and runtime implemented primarily in Rust, with internal AOT/JIT/interpreter modules and Lean 4 models for selected invariants. |
| **Category** | Open-source compiler toolchain, CLI utility, native runtime, and planned Node.js package surface. |
| **Core Technology** | Rust 2024 (`rust-version = 1.97.1`), TypeScript 7.0.2 compatibility oracle (`package.json`), Node.js-hosted execution surfaces, Lean 4 specifications (`formal/lean`), and vendored libuv (`vendor/libuv-1.52.1`). |
| **Pricing / Licensing** | Open source under the MIT License (`Cargo.toml`). |
| **Operational Status** | Pre-release. The `bamti` and `bamti-cli` 0.1.0 wrapper packages are published, but their referenced platform binary packages are unavailable. Source 0.2.0 native artifacts are not published. |

## Developer Persona

| Field | Maintainer Grounding & Scope |
|-------|------------------------------|
| **Primary Role** | Systems engineers, compiler developers, language runtime authors, and TypeScript/Rust platform engineers. |
| **Seniority** | *Hypothesis*: Senior, Staff, and Principal Engineers or Infrastructure Architects evaluating alternative TypeScript runtimes and fast native compilation pipelines. |
| **Company Size** | *Unknown*: No production customer telemetry or user database exists in the repository. |
| **Industry Verticals** | *Unknown*: Repository evidence provides no vertical segmentation data. |
| **Tech Stack** | Rust 1.97.1+, TypeScript 7.0.2 as the compatibility oracle, Node.js, Lean 4, and native C toolchains used by AOT linking. |
| **Decision Authority** | Toolchain maintainers, platform engineers, open-source contributors, and compiler researchers. |

*Explicit Unknowns*: Actual developer demographics, developer team sizes, enterprise adoption percentages, and user geographic distribution are entirely unknown from repository evidence.

## Where They Hang Out

| Channel Category | Known Repository Anchors & Ecosystem Hypotheses |
|------------------|-------------------------------------------------|
| **Code & Issue Tracking** | Primary channel: GitHub repository `https://github.com/metaphorics/bamTiScript` (`Cargo.toml`). |
| **Communities** | *Hypothesis*: `r/rust`, `r/typescript`, compiler design Discord/Slack communities, Rust user groups. |
| **Aggregators & Content** | *Hypothesis*: Hacker News (`news.ycombinator.com`), Lobsters (`lobste.rs`), Rust Weekly, TypeScript release tracking blogs. |
| **Events** | *Hypothesis*: Systems programming conferences (RustConf, EuroRust, TSConf). |

*Explicit Unknowns*: The repository contains no links or records for official Discord servers, Slack channels, subreddit moderation, or social media accounts (X/Twitter, LinkedIn). All non-GitHub channels remain unverified hypotheses.

## Problems & Pain Points

| Level | Evidence-Grounded Description |
|-------|-------------------------------|
| **Functional** | *Hypothesis*: Developers evaluating this project want TypeScript checking and native execution in one toolchain. The repository does not contain user research that establishes their current latency, memory, or CI pain. |
| **Emotional** | *Unknown*: The repository contains no interviews, surveys, or support records from which to infer emotional pain. |
| **Situational** | *Hypothesis*: Evaluation occurs when a team is comparing TypeScript compiler or runtime architectures, native execution, stricter diagnostics, or machine-checked models. |

*Explicit Unknowns*: External problem statements and verbatim complaint logs do not exist in the repository. Resource diagnostics such as `BAMTS-R001` and `BAMTS-R002` describe bamTiScript's own operating limits, not user pain.

## Current Alternatives

| Alternative | Role & Distinction Relative to bamTiScript |
|-------------|--------------------------------------------|
| **`tsc` (Official TypeScript Compiler)** | Compatibility oracle (`typescript: 7.0.2` in the root `package.json`). bamTiScript is a separate pre-release implementation and does not yet claim complete compatibility. |
| **Node.js / V8** | Current execution oracle and host surface for corpus comparisons. bamTiScript implements separate bytecode, runtime, and code-generation crates. |
| **Bun / Deno** | Alternative JavaScript and TypeScript runtimes. No repository evidence supports a performance or compatibility comparison yet. |
| **swc / oxc / other native toolchains** | Native parser and compiler alternatives. No repository evidence supports a feature-completeness or performance comparison yet. |

*Explicit Unknowns*: User switching behavior, comparative market share, and external developer sentiment toward alternatives are unknown from repository data.

## Key Differentiators

| Dimension | Grounded Technical Feature |
|-----------|----------------------------|
| **Technical Safety** | The workspace forbids unsafe Rust by default. Native code generation, host export, verification, FFI, and Node-API modules contain documented scoped exceptions. This policy is not an end-to-end memory-safety guarantee. |
| **Formal Verification** | Lean 4 formal proofs validating bytecode execution semantics, ABI correctness, and JIT lifecycle invariants (`formal/lean/`). |
| **Execution Architecture** | The source tree contains AOT, JIT, and interpreter implementations; execution-target selection is not exposed by the public CLI. |
| **Exact Semantics** | Exact ECMAScript UTF-16 string compliance model (`docs/solutions/architecture-patterns/exact-ecmascript-utf16-strings.md`). |
| **Developer Experience (DX)** | TypeScript-compatible CLI options and project configuration; postinstall-free npm platform shims (`npm/bamti-cli/README.md`). |

*Explicit Unknowns*: Verified production benchmark multipliers (e.g., "10x faster than tsc") cannot be claimed because performance benchmarks (`perf/benchmarks.toml`) represent internal testing fixtures rather than published production measurements.

## Verbatim Developer Language

Because the repository has no external issue tracker export, user survey, or community chat logs, external developer quotes **must not be fabricated**. The following terms represent verified internal CLI and diagnostic vocabulary:

- **CLI Operations**: `--noEmit`, `--project`, `--build`, and direct source-file compilation. `--init` appears in `--help` but currently fails in an empty directory (`TS5083`) and does not write `tsconfig.json`.
- **Compiler Options**: `--allowJs`, `--checkJs`, `--jsx preserve`, and other TypeScript 7.0.2 option names.
- **Diagnostic Presentation**: `--pretty` and `--pretty false`.
- **Budget Breach Identifiers**: `BAMTS-R001` (file source byte limit exceeded), `BAMTS-R002` (session total source byte limit exceeded).
- **Frontend Diagnostic Codes**: `BAMTS-C004` (and related semantic checking error codes).

*Explicit Unknowns*: Outside developer quotes, praise, tweets, or verbatim feature requests are completely absent from repository evidence.

## Technical Trust Signals

| Signal Type | Repository Grounding & Status |
|-------------|-------------------------------|
| **Code Quality & Safety** | Workspace lint rule `[workspace.lints.rust] unsafe_code = "forbid"` (`Cargo.toml`). |
| **Formal Correctness** | Lean 4 proof specifications in `formal/lean/Bytecode/Verify.lean` and `JitLifecycle.lean`. |
| **Transparency** | The npm package README distinguishes the published JavaScript wrapper from unavailable native platform artifacts. |
| **Test Coverage** | Conformance and differential verification suites in `crates/bamts-verification`. |

*Explicit Unknowns*: GitHub star count, npm download numbers, third-party security audit reports, and corporate sponsors are not established by repository evidence.

## Conversion Actions

| Funnel Stage | Maintainer & Contributor Action |
|--------------|---------------------------------|
| **Awareness / Discovery** | Review repository architecture, `Cargo.toml`, and compiler rules (`RULES.md`). |
| **Consideration / Evaluation** | Clone the workspace; inspect Lean proofs (`formal/lean/`); examine the public CLI argument contract (`crates/bamts-cli/src/cli/tsc_args.rs`). |
| **Local Verification** | Execute Rust workspace checks (`cargo check --workspace`) and test suites (`cargo test --workspace`). |
| **Future Trial (Post-Release)** | Install the planned `bamti-cli` npm package, then run `bamts --noEmit <file.ts>` or `bamts <file.ts> --outDir out`. |
| **Future Integration** | Use the `bamti` Node package after its native platform artifacts are implemented and published. |

## Voice & Tone

| Dimension | Setting & Maintainer Rule |
|-----------|---------------------------|
| **Formality** | **Professional / Academic**: Precise technical language without informal slang. |
| **Technicality** | **Deep Technical**: Direct reference to AST nodes, bytecode verification, Lean semantics, UTF-16 representation, and Rust workspace layout. |
| **Personality** | **Rigorous and Transparent**: Separate behavior verified in the current tree from compatibility targets and release plans. |
| **Humor** | **Serious**: Zero marketing hyperbole or playful detours. |

*Maintainer Rule*: Maintain documentation as evidence-backed internal records. State facts verified in code, label all unproven items as hypotheses, and explicitly identify unknown parameters where repository evidence is silent.
