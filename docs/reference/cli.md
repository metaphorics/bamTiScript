# Command line interface (CLI) reference

`bamts` is the primary binary interface for the bamTiScript TypeScript compiler and runtime.

## Command help output

The CLI help text is built directly into `bamts`:

```text
bamts - TypeScript compiler and runtime for Rust

USAGE:
    bamts [SUBCOMMAND] [OPTIONS] [ENTRYPOINT] [-- PROGRAM_ARGS...]

SUBCOMMANDS:
    check       Type-check source files without emitting output artifacts
    compile     Compile TypeScript source files to output artifacts
    run         Execute TypeScript/JavaScript program
    explain     Explain a lint rule and its sound alternative

OPTIONS:
    -c, --compile               Select compile mode
    -r, --run                   Select run mode
        --check                 Select check mode

        --target <aot|jit>      Set execution target (aot or jit)
        --aot                   Alias for --target aot
        --jit                   Alias for --target jit

        --js-compat             Enable JavaScript compatibility options
        --compat <MODE>         Set JS compatibility mode (standard, esnext, es2022, node, strict, loose)
        --allow-js              Allow parsing .js and .jsx files
        --check-js              Enable type checking on JavaScript files
        --jsx-preserve          Preserve JSX constructs in emitted code

    -o, --output <FILE>         Specify output file path
        --out-dir <DIR>         Specify output directory
    -d, --emit-declarations     Emit declaration files (.d.ts)
    -m, --source-maps           Generate source maps (.map)

        --diagnostics-format <FMT>  Format for diagnostics (text, pretty, json, github, compact)
        --format <FMT>          Alias for --diagnostics-format
        --json                  Alias for --diagnostics-format json
        --pretty                Alias for --diagnostics-format pretty

    -A, --allow <RULE>          Set a rule or group to allow
    -W, --warn <RULE>           Set a rule or group to warn
    -D, --deny <RULE>           Set a rule or group to deny
    -F, --forbid <RULE>         Set a rule or group to forbid
        --strict                Enable strict lint profile
        --pedantic              Enable pedantic lint profile
        --error-limit <N>       Render at most N diagnostics (default: 50)

    -h, --help                  Print help information
    -V, --version               Print version information
```

---

## Subcommands

### `check`
Type-checks TypeScript and enabled JavaScript source files without writing output artifacts.

```bash
target/release/bamts check src/main.ts
```

### `compile`
Compiles a TypeScript program into a native executable using ahead-of-time (AOT) compilation.

```bash
target/release/bamts compile --target aot src/main.ts -o bin/main
```

### `run`
Type-checks and executes a TypeScript or JavaScript program directly.

```bash
# Execute using JIT compilation
target/release/bamts run --target jit src/main.ts

# Execute using AOT compilation and native linking
target/release/bamts run --target aot src/main.ts
```

### `explain`
Prints the rationale, sound alternative, and silence flag for a lint rule code or name, or an explanation for a resource budget code.

```bash
target/release/bamts explain BAMTS-W017
target/release/bamts explain BAMTS-R001
```

---

## Execution targets

bamTiScript supports two execution targets:

* JIT (`--target jit` or `--jit`): Lowers the program to bytecode and executes it through the JIT path.
* AOT (`--target aot` or `--aot`): Produces a native object, links it with the host runtime through the system C toolchain, and either runs or writes the executable.

---

## Compilation restrictions

The `compile` subcommand has these boundaries:

1. One entrypoint: `compile` accepts one entrypoint per invocation. The entrypoint can load other modules.
2. AOT only: `compile --target jit` fails with exit code `1` because the JIT target does not produce a persistent artifact.
3. Native executable output: Declaration generation and source-map output are rejected for native compilation.

---

## Exit codes

`bamts` reserves `0` for successful compiler operations, `1` for compiler or driver failures, and `2` for argument or configuration usage errors. A successful `run` invocation returns the executed program's completion status, so other status codes are possible.

| Status | Meaning |
| :--- | :--- |
| `0` | The compiler operation succeeded, or the executed program completed with status 0. Warnings alone do not change it. |
| `1` | A frontend, budget, lowering, runtime, host, linking, output, or I/O failure occurred; an executed program can also select status 1. |
| `2` | CLI argument or configuration usage failed, including invalid or conflicting options, a missing entrypoint, or lowering a forbidden lint level. |
| Other non-zero | A `run` target completed with that program-selected status. |

---

## Lint flags and `bamts.toml`

bamTiScript applies lint configuration from project settings and command-line overrides.

### Configuration ownership
* `bamts.toml`: Project lint defaults use `[lints.groups]` and `[lints.rules]` with `allow`, `warn`, `deny`, or `forbid` values.
* Profiles: `--strict` and `--pedantic` select preset lint levels.
* Rule flags: `-A`, `-W`, `-D`, and `-F` apply command-line rule or group levels.

### Severity flags
* `-A, --allow <RULE>`: Suppress diagnostics for a rule or rule group.
* `-W, --warn <RULE>`: Emit warning diagnostics for a rule or rule group.
* `-D, --deny <RULE>`: Emit error diagnostics for a rule or rule group.
* `-F, --forbid <RULE>`: Set a rule to forbid.

### Forbidden rule behavior
A `forbid` level cannot be lowered by a later override. An attempted downgrade exits with code `2`.

For a complete list of lint rules and rule groups, see [`../../crates/bamts-compiler/RULES.md`](../../crates/bamts-compiler/RULES.md).

---

## `tsconfig.json` subset

`bamts` reads a strict JSON-with-comments view of selected project fields:

* root `files`, `include`, `exclude`, and `extends`
* `compilerOptions.target`, `module`, `moduleResolution`, and `jsx`
* `compilerOptions.strict`, `strictNullChecks`, `noImplicitAny`, and `alwaysStrict`
* `compilerOptions.allowJs`, `checkJs`, and `resolveJsonModule`
* `compilerOptions.baseUrl`, `paths`, `rootDir`, and `outDir`

The current loader records `extends` but does not resolve an inherited configuration.
TypeScript strictness options control checker behavior; they do not set bamTiScript lint
levels. Configure lint groups and rules through `bamts.toml` or the CLI lint flags.

---

## Resource budgets (`BAMTS-R001` and `BAMTS-R002`)

The compiler frontend enforces hard source-input budgets:

* `BAMTS-R001` (`source-too-large`): One source file can contain at most **16 MiB** (16,777,216 bytes).
* `BAMTS-R002` (`session-too-large`): One compilation session can load at most **256 MiB** (268,435,456 bytes) of source text.

---

## Related documentation

* [Diagnostics reference](./diagnostics.md)
* [Lint rules reference](../../crates/bamts-compiler/RULES.md)
* [UTF-16 string architecture](../solutions/architecture-patterns/exact-ecmascript-utf16-strings.md)
