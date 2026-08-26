# Command line interface (CLI) reference

`bamts` is the TypeScript 7.0.2-compatible command-line interface. It accepts `tsc` option names and short names, response files, project configuration, and build mode.

## Help and version

```bash
bamts --help
bamts --help --all
bamts --version
```

The concise help identifies the program as `tsc: The TypeScript Compiler - Version 7.0.2`. `bamts`, `bamti`, `tsc`, and executable paths ending in those names are accepted as the program token.

## Common commands

```bash
# Compile the project selected by tsconfig.json in the working directory
bamts

# Compile explicit source files without loading tsconfig.json
bamts app.ts util.ts

# Type-check without writing output files
bamts --noEmit

# Compile the project selected by a configuration path
bamts --project ./path/to/tsconfig.json

# Build a composite project and its references
bamts --build

# Create a recommended tsconfig.json
bamts --init
```

With no explicit source files, `bamts` discovers `tsconfig.json` from the working directory. Use `--project` (`-p`) for a specific configuration or `--build` (`-b`) for composite projects. When source files are given directly, the CLI uses command-line options instead of loading `tsconfig.json`. `--init` is accepted by help text, but an empty directory currently fails with `TS5083` and does not write `tsconfig.json`.

## Output options

The parser accepts TypeScript output option names including `--outDir`, `--outFile`, `--declaration` (`-d`), `--declarationMap`, `--sourceMap`, and `--noEmit`. Direct source-file compilation with `--outDir` writes a native host executable. `--declaration` and `--sourceMap` on that path currently fail with `TS5047` (`Declaration and source-map emission require canonical compiler outputs`). Project mode applies the `tsconfig.json` equivalents of these options; it does not emit JavaScript.

## Parsing behavior

The parser accepts TypeScript 7.0.2 compiler option names and rejects unknown options instead of silently discarding them. Boolean options accept an omitted value, `true`, `false`, or `null`. Response files use the `@path` form.

The previous `check`, `run`, `compile`, `explain`, `--target jit`, and `--target aot` interface is not part of the current public CLI.

## Exit codes

The CLI preserves TypeScript-compatible status classes:

| Status | Meaning |
|---:|---|
| 0 | Success |
| 1 | Diagnostics were generated and output was emitted |
| 2 | Diagnostics prevented output |
| 3 | One or more projects are out of date in build-status mode |
| 4 | Build outputs were generated successfully |

Argument and configuration failures are rendered as TypeScript diagnostics and mapped through the same status model.

## Resource budgets

The compiler frontend enforces source-input budgets:

* `BAMTS-R001` (`source-too-large`): one source file can contain at most **16 MiB** (16,777,216 bytes).
* `BAMTS-R002` (`source-budget-exceeded`): one session can load at most **256 MiB** (268,435,456 bytes) across all source files.

## Related documentation

* [Diagnostics reference](./diagnostics.md)
* [Source-build quickstart](../tutorials/quickstart.md)
