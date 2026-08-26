# AST and Diagnostic Change-Window Protocol

## Purpose

This protocol coordinates modifications to shared Abstract Syntax Tree (AST) definitions, scanner tokens, parser routines, root checker interfaces, and diagnostic tables in the `bamTiScript` workspace.

Maintainers and subagents must follow this protocol to prevent concurrent write collisions, broken pattern matches, invalid UTF-16 source spans, and diagnostic code overlap.

## Shared Root Modules

Only one announced owner may modify shared compiler root files during a wave. The shared root files are:

| File Path | Functional Responsibility |
| :--- | :--- |
| `crates/bamts-compiler/src/syntax.rs` | AST node structures, `NodeKind`, `TokenKind`, `SourceFile`, `Statement`, `Expression` |
| `crates/bamts-compiler/src/scanner.rs` | Lexer token recognition, keyword tables, literal scanners |
| `crates/bamts-compiler/src/parser.rs` | Grammar parsing, syntax error emission, tree construction |
| `crates/bamts-compiler/src/checker.rs` | Root semantic checker engine and type environment orchestration |
| `crates/bamts-compiler/src/diagnostic.rs` | Core diagnostic data types, `DiagnosticCode`, `DiagnosticSeverity` |
| `crates/bamts-compiler/src/diagnostics_parser.rs` | Mapping from native lexer/parser codes to TypeScript parse codes |
| `crates/bamts-compiler/src/generated/diagnostic_messages.rs` | Generated diagnostic messages table from TypeScript authority JSON |
| `crates/bamts-compiler/src/tsc_directives.rs` | Directive parser (`// @flags`, `@filename` splits) |

Child modules in `crates/bamts-compiler/src/binder/`, `crates/bamts-compiler/src/checker/`, `crates/bamts-compiler/src/emitter/`, `crates/bamts-compiler/src/project/`, and `crates/bamts-cli/src/cli/` are owned by individual task leaves and do not require shared root locks.

## Window Ownership Announcement

Before you edit shared root files, announce ownership to all peers over the communication hub.

### Acquisition Notice Format

Send a message over `hub` with the following parameters:
- `to`: `"all"`
- `message`: `ACQUIRE_AST_WINDOW: wave=<WAVE_ID> owner=<AGENT_OR_MAINTAINER> files=<FILE_LIST> reason=<SUMMARY>`

Example:
```json
{
  "op": "send",
  "to": "all",
  "message": "ACQUIRE_AST_WINDOW: wave=W2 owner=AstWindowProtocol files=crates/bamts-compiler/src/syntax.rs,crates/bamts-compiler/src/parser.rs reason=Add import attribute syntax nodes"
}
```

## Content Hashes and Fresh Reads

Record the SHA-256 hash of target files before making edits:

```bash
sha256sum crates/bamts-compiler/src/syntax.rs crates/bamts-compiler/src/scanner.rs crates/bamts-compiler/src/parser.rs crates/bamts-compiler/src/checker.rs
```

Always perform fresh line-anchored reads before modifying files. Do not use elided rows (`…`, `..`), pagination footers, or snapshot headers as write input.

## AST and Span Invariants

All changes to AST nodes and source coordinates must preserve the following invariants:

### Append-Only Discriminants

- Append new grammar nodes to `NodeKind` in `crates/bamts-compiler/src/syntax.rs`.
- Append new token variants to `TokenKind` in `crates/bamts-compiler/src/syntax.rs`.
- Do not reorder or remove existing enum variants.
- Update pattern matching in `scanner.rs`, `parser.rs`, `binder/`, `checker/`, and `emitter/` for all new variants. Avoid non-exhaustive wildcard defaults in critical compilation passes.

### UTF-16 Code-Unit Range Invariants

- All source positions are UTF-16 code-unit offsets using `Utf16Pos` (`crates/bamts-compiler/src/source.rs`).
- All node and token spans use `TextRange` (`crates/bamts-compiler/src/source.rs`).
- Every `TextRange` must satisfy `start <= end`.
- Never assign raw UTF-8 byte offsets directly to `TextRange`.
- Never split Unicode surrogate pairs (`0xD800..=0xDBFF` and `0xDC00..=0xDFFF`) across range boundaries.

## Diagnostic Code Bands and Catalog Generation

### Diagnostic Bands

Allocate diagnostic codes strictly within the assigned bands:

| Band | Subsystem | Definition Location | Description |
| :--- | :--- | :--- | :--- |
| `BAMTS-L001` .. `BAMTS-L099` | Lexer | `crates/bamts-compiler/src/scanner.rs` | Scanner and lexical error codes. |
| `BAMTS-P001` .. `BAMTS-P999` | Parser | `crates/bamts-compiler/src/parser.rs` | Grammar and syntax error codes. |
| `BAMTS-C001` .. `BAMTS-C999` | Checker | `crates/bamts-compiler/src/checker/` | Semantic and type checking error codes. |
| `BAMTS-W001` .. `BAMTS-W999` | Warnings | `crates/bamts-compiler/src/diagnostic.rs` | Compiler warnings and lint diagnostics. |
| `BAMTS-E001` .. `BAMTS-E999` | Engine | `crates/bamts-compiler/src/diagnostic.rs` | Pipeline and engine execution errors. |
| `TS1000` .. `TS18000+` | Parity | `crates/bamts-compiler/src/generated/diagnostic_messages.rs` | Upstream TypeScript 4-digit and 5-digit error codes. |

### Diagnostic Catalog Verification

The diagnostic table `crates/bamts-compiler/src/generated/diagnostic_messages.rs` is generated from `target/authority/typescript-7.0.2-tests/src/compiler/diagnosticMessages.json` by `crates/bamts-verification/src/diagnostic_catalog.rs`.

Do not edit `crates/bamts-compiler/src/generated/diagnostic_messages.rs` manually. The cargo library test is the current consistency check:

```bash
cargo test -p bamts-verification --lib diagnostic_catalog
```

## Child Leaf Handoff Contract

Subagents working in child directories (`binder/`, `checker/`, `emitter/`, `project/`) submit integration requests to the Window Owner when root modifications are needed.

The handoff payload must contain:
1. `leaf_id`: The completion leaf identifier (for example, `B1.2`).
2. `required_nodes`: New `NodeKind` or `TokenKind` variants and their fields.
3. `required_diagnostics`: Specific diagnostic codes requested from the appropriate band.
4. `child_patch`: Path or contents of the isolated child module changes.

The Window Owner integrates the variants into shared root files, verifies compilation, and notifies the leaf owner.

## Operational Rules

1. **No Non-Owner Reverts**: Non-owners must never execute `git restore`, `git checkout`, or `git reset` on shared root files (`syntax.rs`, `scanner.rs`, `parser.rs`, `checker.rs`, `diagnostic.rs`, `diagnostics_parser.rs`, `generated/diagnostic_messages.rs`, `tsc_directives.rs`).
2. **Anchored Edits**: Apply edits using exact line numbers and target strings read from recent snapshots.
3. **No Pipe Masking**: Run verification commands directly without piping to `head` or `tail`.

## Verification Ladder

Execute the verification steps in order:

```bash
# 1. Compiler crate check
cargo check -p bamts-compiler

# 2. Compiler unit tests
cargo test -p bamts-compiler --lib

# 3. Diagnostic catalog verification
cargo test -p bamts-verification --lib diagnostic_catalog

# 4. Compiler lints
cargo clippy -p bamts-compiler --all-targets -- -D warnings
```

If the leaf gate includes mutation verification (G5 aspect), apply a temporary mutation to verify that the check fails, then restore the correct implementation before completing the task.

## Window Release Announcement

After you verify all edits and capture post-edit SHA-256 hashes, release the window by broadcasting a message over `hub`.

### Release Notice Format

- `to`: `"all"`
- `message`: `RELEASE_AST_WINDOW: wave=<WAVE_ID> owner=<AGENT_OR_MAINTAINER> files=<FILE_LIST> status=VERIFIED pre_hashes=<PRE_HASHES_OR_EVIDENCE_PATH> post_hashes=<POST_HASHES_OR_EVIDENCE_PATH>`

Example:
```json
{
  "op": "send",
  "to": "all",
  "message": "RELEASE_AST_WINDOW: wave=W2 owner=AstWindowProtocol files=crates/bamts-compiler/src/syntax.rs,crates/bamts-compiler/src/parser.rs status=VERIFIED pre_hashes=syntax:a1b2c3d4,parser:e5f6a7b8 post_hashes=syntax:12345678,parser:9abcdef0"
}
```
