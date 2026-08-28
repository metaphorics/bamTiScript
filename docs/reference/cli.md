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

# Compile explicit source files with command-line options
bamts --ignoreConfig app.ts util.ts

# Type-check without writing output files
bamts --noEmit

# Compile the project selected by a configuration path
bamts --project ./path/to/tsconfig.json

# Build a composite project and its references
bamts --build

# Create a recommended tsconfig.json
bamts --init
```

With no explicit source files, `bamts` discovers `tsconfig.json` in the working directory. Use `--project` (`-p`) for a specific configuration or `--build` (`-b`) for composite projects.

When direct source files run in a directory that contains `tsconfig.json`, the CLI returns `TS5112`. Add `--ignoreConfig` to compile those files with command-line options instead.

`bamts --init` writes the TypeScript 7.0.2 recommended configuration when `tsconfig.json` is absent. If it already exists, the command returns `TS5054` and leaves it unchanged. Initialization ignores source, project, pretty, and target options.

## Output options

The parser accepts TypeScript output option names including `--outDir`, `--outFile`, `--declaration` (`-d`), `--declarationMap`, `--sourceMap`, and `--noEmit`.

Direct source-file compilation writes a native host executable. With `--outDir <dir>`, it writes `<dir>/<entrypoint-name>`; otherwise it writes beside the source file. `--declaration` and `--sourceMap` fail closed with `TS5047` on that path because native compilation has no canonical mapping for those outputs.

Project mode writes JavaScript and declaration outputs from the corresponding `tsconfig.json` options.

## Parsing behavior

The parser accepts supported TypeScript 7.0.2 compiler option names and rejects unknown options instead of silently discarding them. Boolean options accept an omitted value, `true`, `false`, or `null`; on the command line, `null` leaves the flag unset. Response files use the `@path` form.

Project mode accepts `--target` values from `es6` through `esnext`. `--target es5` returns `TS5108`. Direct native compilation fails closed with `TS5047` for accepted options that have no canonical native mapping, including `--target`.

The previous `check`, `run`, `compile`, `explain`, `--target jit`, and `--target aot` interface is not part of the current public CLI.

## Exit codes

The CLI uses TypeScript-compatible status classes 0 through 4 and reserves 5 for native compilation constraints.

| Status | Meaning |
|---:|---|
| 0 | Success |
| 1 | Diagnostics were generated and output was skipped |
| 2 | Diagnostics were generated and output was emitted |
| 3 | Invalid project output prevented emission |
| 4 | A project-reference cycle prevented emission |
| 5 | The requested behavior has no canonical native mapping |

TypeScript-format parse, project, and compiler diagnostics are written to stdout. Argument and configuration failures use the same status model.

## Resource budgets

The compiler frontend enforces source-input budgets:

* `BAMTS-R001` (`source-too-large`): one source file can contain at most **16 MiB** (16,777,216 bytes).
* `BAMTS-R002` (`source-budget-exceeded`): one session can load at most **256 MiB** (268,435,456 bytes) across all source files.

## Related documentation

* [Diagnostics reference](./diagnostics.md)
* [Source-build quickstart](../tutorials/quickstart.md)
