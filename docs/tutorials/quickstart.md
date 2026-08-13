# Source-build quickstart

This tutorial walks through building bamTiScript from source on Linux x64, type-checking a TypeScript program, running it with both JIT and AOT targets, compiling it to a native executable, and exploring the built-in diagnostic explanation tool.

## Prerequisites

Before building bamTiScript from source, check that your system meets these requirements:

- Operating system: Linux x64 (the target assumed by this guide)
- Rust toolchain: the repository pins Rust 1.97.1 with Cargo, rustfmt, and Clippy
- Host C toolchain: a compiler driver selected by `$CC`, defaulting to `cc`, required only for AOT steps

> **Note:** `bamti` (the in-process Node 24+ interface) and `bamti-cli` (the standalone CLI transport) 0.1.0 are published on npm, but currently published packages do not contain prebuilt native binary artifacts. The source implementation of native Node-API bindings (`bamts-napi`) is unpublished, and real five-target release validation is blocked by GitHub billing. Build the working CLI binary from source with Cargo.

## 1. Clone the repository

Clone the bamTiScript repository and move into the project directory:

```bash
git clone https://github.com/metaphorics/bamTiScript.git
cd bamTiScript
```

## 2. Build the CLI binary

Build the release binary using Cargo:

```bash
cargo build --release -p bamts-cli
```

Once the build finishes, the executable is available at `target/release/bamts`.

## 3. Create a TypeScript source file

Create `hello.ts`:

```bash
cat > hello.ts <<'EOF'
const message: string = "hello from bamts";
process.stdout.write(`${message}\n`);
EOF
```

## 4. Type-check the source

Run the static type checker without emitting code:

```bash
target/release/bamts check hello.ts
```

When type checking reports no errors, `bamts` exits with code `0`. Warnings do not change the exit code.

## 5. Execute with JIT

Execute your TypeScript source directly using the JIT engine:

```bash
target/release/bamts run --target jit hello.ts
```

Output:
```text
hello from bamts
```

You have now run the program through bamTiScript's JIT path.

## 6. Execute with AOT

Next, run the program using the AOT execution pipeline (requires a host C toolchain):

```bash
target/release/bamts run --target aot hello.ts
```

Output:
```text
hello from bamts
```

## 7. Compile a native executable

Compile `hello.ts` into a native executable named `hello`:

```bash
target/release/bamts compile --target aot -o hello hello.ts
```

Execute the output binary directly:

```bash
./hello
```

Output:
```text
hello from bamts
```

> **Note:** AOT compilation accepts one entrypoint and targets the local host architecture. Cross-compilation is not supported.

## 8. Inspect a diagnostic explanation

bamTiScript includes built-in explanations for lint rules and diagnostic codes. Query the explanation for rule `BAMTS-W017`:

```bash
target/release/bamts explain BAMTS-W017
```

**Observed output**:
```text
BAMTS-W017 (explicit-any)
rationale: Explicit any disables type checking at the annotated boundary.
sound alternative: Use unknown and narrow it before use.
silence: -A explicit-any
```

## Next steps and further reading

- [Project README](../../README.md): Project scope, current capabilities, and limits
- [Architecture](../explanation/architecture.md): Compiler, runtime, and host boundaries
- [CLI reference](../reference/cli.md): Commands, flags, configuration, and exit codes
- [Diagnostics reference](../reference/diagnostics.md): Diagnostic families and output formats
- [Verification evidence](../explanation/verification.md): What the current evidence proves and does not prove
- [Contributing guide](../../CONTRIBUTING.md): Source organization and unpiped verification gates
- [Compiler rules catalog](../../crates/bamts-compiler/RULES.md): Generated lint-rule reference
- [Exact ECMAScript UTF-16 strings](../solutions/architecture-patterns/exact-ecmascript-utf16-strings.md): String representation and boundary rules
