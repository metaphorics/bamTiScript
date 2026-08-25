# Diagnostics reference

The bamTiScript compiler emits structured diagnostics during lexical analysis, parsing, semantic analysis, type checking, and resource checking.

## Diagnostic code families

Diagnostic codes use the format `BAMTS-<FAMILY><NUMBER>`. Codes are grouped into five functional families:

| Family | Name | Scope and description |
| :--- | :--- | :--- |
| `BAMTS-L` | Lexer | Scans source text into tokens. Reports malformed lexical input. |
| `BAMTS-P` | Parser | Builds the abstract syntax tree and reports grammar errors. |
| `BAMTS-C` | Checker | Binds symbols, evaluates types, and reports semantic errors. |
| `BAMTS-W` | Warning | Reports configurable lint rules. |
| `BAMTS-R` | Resource | Enforces source-input budgets. |

For a complete list of lint rules and descriptions in the `BAMTS-W` family, see [`../../crates/bamts-compiler/RULES.md`](../../crates/bamts-compiler/RULES.md).

---

## Output formats

The CLI supports five diagnostic output formats configured via `--diagnostics-format <FMT>` (or `--format`, `--json`, `--pretty`):

* `text`: Default line-oriented format suitable for standard terminal reading (`file:line:col: severity[code]: message`).
* `pretty`: Rich visual format with line numbers, source code excerpts, and UTF-16 aligned caret underlines (`^^^^^`).
* `compact`: Concise line-oriented format omitting diagnostic codes (`file:line:col: severity: message`).
* `json`: Machine-readable JSON array with source, severity, code, message, offset, line, and column fields.
* `github`: GitHub workflow annotation syntax.

---

## Coordinate system and UTF-16 positioning

Diagnostic line and column coordinates are 1-based. Columns and source offsets count UTF-16 code units.

* Line numbers start at line `1`.
* Column numbers start at column `1`.
* Non-BMP characters advance the column by two code units.

For details on the string representation and coordinate mapping, see [`../solutions/architecture-patterns/exact-ecmascript-utf16-strings.md`](../solutions/architecture-patterns/exact-ecmascript-utf16-strings.md).

---

## Error limits and truncation

The compiler caps the number of rendered diagnostics at **50** by default (`--error-limit 50`).

When diagnostics exceed the limit, the renderer keeps the canonical first `N` diagnostics. Line-oriented formats append an elision notice:

```text
note: 5 diagnostic(s) elided after limit 50; raise with `--error-limit`
```

Structured formats retain valid JSON or GitHub annotation output and omit this text notice.

---

## TypeScript diagnostic mapping

[`verification/diagnostic-code-map.json`](../../verification/diagnostic-code-map.json) records mappings only when the repository has TypeScript evidence for an equivalent rule. Unmapped bamTiScript diagnostics remain explicit. A mapping record contains the bamTiScript code, TypeScript code, test snippet, and observed `tsc` output.

---

## Output format examples

Given an input file `example.ts` containing:

```typescript
const value: number = "not a number";
```

Executing `target/release/bamts check -A unused-local --format <FMT> example.ts` renders the following output. The examples shorten the machine-dependent absolute source path to `/path/to/example.ts`.

### Text format

```text
/path/to/example.ts:1:7: error[BAMTS-C004]: Initializer type is not assignable to the annotated type.
```

### Pretty format

```text
error[BAMTS-C004]: Initializer type is not assignable to the annotated type.
 --> /path/to/example.ts:1:7
  |
1 | const value: number = "not a number";
  |       ^^^^^
```

### Compact format

```text
/path/to/example.ts:1:7: error: Initializer type is not assignable to the annotated type.
```

### JSON format

```json
[{"sourceId":0,"source":"/path/to/example.ts","severity":"error","code":"BAMTS-C004","message":"Initializer type is not assignable to the annotated type.","startOffset":6,"endOffset":11,"line":1,"column":7,"endLine":1,"endColumn":12}]
```

### GitHub format

```text
::error file=/path/to/example.ts,line=1,col=7,endLine=1,endColumn=12::Initializer type is not assignable to the annotated type.
```

---

## Related documentation

* [CLI reference](./cli.md)
* [Lint rules reference](../../crates/bamts-compiler/RULES.md)
* [Diagnostic code map](../../verification/diagnostic-code-map.json)
* [UTF-16 string architecture](../solutions/architecture-patterns/exact-ecmascript-utf16-strings.md)
