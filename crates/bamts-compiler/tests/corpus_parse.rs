//! Corpus-driven scanner and parser conformance.
//!
//! Sources are discovered from the checked corpus manifest and the project
//! specs it names, without a TOML dependency: a strict minimal line parser
//! reads the exact subset of TOML those checked files use (table headers,
//! quoted strings, and string arrays) and rejects anything else.
//!
//! Three properties are asserted for every discovered source:
//!
//! * *Totality*: [`scan`] and [`parse`] return for every checked source, and a
//!   panic in either is a test failure by construction.
//! * *Token/source identity*: both token streams tile the source exactly once,
//!   in order, with no gap or overlap, and the concatenated lexemes reproduce
//!   the source byte for byte. Forward progress follows: every non-missing
//!   token covers at least one UTF-16 code unit.
//! * *Range and diagnostic well-formedness*: every token and diagnostic range
//!   is a valid boundary pair of the same source, the parser's `SourceFile`
//!   keeps the scanner's file identity, script kind, text, and EOF anchor, and
//!   the diagnostics reachable through `SourceFile` are exactly the canonically
//!   ordered diagnostics of the returned `Recovered`.
//!
//! Whole corpus files are *not* required to be diagnostic-free. Zero
//! diagnostics is asserted only for the focused, corpus-derived construct table
//! at the bottom of this file, where the exact syntax under test is written out.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use bamts_compiler::{
    diagnostic::Diagnostic,
    parser::parse,
    scanner::scan,
    source::{ScriptKind, SourceId, SourceText},
    syntax::{NodeKind, Token, TokenKind},
};

// ---------------------------------------------------------------------------
// Strict minimal TOML reader
// ---------------------------------------------------------------------------

/// The only value shapes the checked corpus files use.
#[derive(Debug)]
enum TomlValue {
    Text(String),
    List(Vec<String>),
    /// A number or boolean. Recorded so unknown keys still parse strictly.
    Scalar,
}

#[derive(Debug)]
struct TomlTable {
    header: String,
    entries: Vec<(String, TomlValue)>,
}

impl TomlTable {
    fn text(&self, key: &str) -> Option<&str> {
        self.entries.iter().find_map(|(name, value)| match value {
            TomlValue::Text(text) if name == key => Some(text.as_str()),
            _ => None,
        })
    }

    fn list(&self, key: &str) -> Option<&[String]> {
        self.entries.iter().find_map(|(name, value)| match value {
            TomlValue::List(items) if name == key => Some(items.as_slice()),
            _ => None,
        })
    }

    fn required_text(&self, path: &Path, key: &str) -> &str {
        self.text(key)
            .unwrap_or_else(|| panic!("{}: missing required string key `{key}`", path.display()))
    }
}

/// Removes an unquoted `#` comment from one line.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (offset, character) in line.char_indices() {
        match character {
            '"' => in_string = !in_string,
            '#' if !in_string => return line[..offset].trim_end(),
            _ => {}
        }
    }
    line
}

/// Returns the unclosed `[` depth of an accumulated array body.
fn bracket_depth(path: &Path, line_number: usize, body: &str) -> usize {
    let mut depth: usize = 0;
    let mut in_string = false;
    for character in body.chars() {
        match character {
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => {
                depth = depth.checked_sub(1).unwrap_or_else(|| {
                    panic!(
                        "{}:{}: unbalanced `]` in array value",
                        path.display(),
                        line_number
                    )
                });
            }
            _ => {}
        }
    }
    assert!(
        !in_string,
        "{}:{}: unterminated string in array value",
        path.display(),
        line_number
    );
    depth
}

/// Collects every quoted string of an array body, rejecting escapes.
fn collect_strings(path: &Path, line_number: usize, body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current: Option<String> = None;
    for character in body.chars() {
        match (&mut current, character) {
            (None, '"') => current = Some(String::new()),
            (None, _) => {}
            (Some(_), '\\') => panic!(
                "{}:{}: escape sequences are not supported by this reader",
                path.display(),
                line_number
            ),
            (Some(_), '"') => items.push(current.take().unwrap_or_default()),
            (Some(text), other) => text.push(other),
        }
    }
    assert!(
        current.is_none(),
        "{}:{}: unterminated string in array value",
        path.display(),
        line_number
    );
    items
}

/// Reads one complete quoted string that spans the rest of a line.
fn single_string(path: &Path, line_number: usize, rest: &str) -> String {
    let items = collect_strings(path, line_number, rest);
    assert_eq!(
        items.len(),
        1,
        "{}:{}: expected exactly one quoted string, found {}",
        path.display(),
        line_number,
        items.len()
    );
    items
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable!("length checked above"))
}

/// Parses the checked subset of TOML used by the corpus manifest and specs.
///
/// The reader is deliberately strict: a line that is not blank, a comment, a
/// table header, or `key = value` fails the test rather than being skipped, so
/// a corpus file that grows a new shape cannot silently drop sources.
fn parse_minimal_toml(path: &Path, text: &str) -> Vec<TomlTable> {
    let mut tables = vec![TomlTable {
        header: String::new(),
        entries: Vec::new(),
    }];
    let mut lines = text.lines().enumerate();

    while let Some((index, raw)) = lines.next() {
        let line_number = index + 1;
        let line = strip_comment(raw.trim()).trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && !line.contains('=') {
            assert!(
                line.ends_with(']'),
                "{}:{line_number}: malformed table header `{line}`",
                path.display()
            );
            tables.push(TomlTable {
                header: line.to_owned(),
                entries: Vec::new(),
            });
            continue;
        }

        let (key, rest) = line.split_once('=').unwrap_or_else(|| {
            panic!(
                "{}:{line_number}: expected `key = value`, found `{line}`",
                path.display()
            )
        });
        let key = key.trim().to_owned();
        let rest = rest.trim();
        assert!(
            !key.is_empty() && !rest.is_empty(),
            "{}:{line_number}: empty key or value",
            path.display()
        );

        let value = if rest.starts_with('"') {
            TomlValue::Text(single_string(path, line_number, rest))
        } else if rest.starts_with('[') {
            let mut body = rest.to_owned();
            while bracket_depth(path, line_number, &body) > 0 {
                let (_, continuation) = lines.next().unwrap_or_else(|| {
                    panic!("{}:{line_number}: unterminated array value", path.display())
                });
                body.push('\n');
                body.push_str(strip_comment(continuation.trim()).trim());
            }
            TomlValue::List(collect_strings(path, line_number, &body))
        } else {
            TomlValue::Scalar
        };

        tables
            .last_mut()
            .unwrap_or_else(|| unreachable!("the root table is always present"))
            .entries
            .push((key, value));
    }

    tables
}

// ---------------------------------------------------------------------------
// Corpus discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Eq, PartialEq)]
struct CorpusSource {
    /// Repository-relative path, the deterministic ordering key.
    relative: String,
    path: PathBuf,
    script_kind: ScriptKind,
    /// Where the corpus declares this file, used in failure messages.
    origin: String,
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("crate directory must live two levels below the workspace root"))
        .to_path_buf()
}

fn read_checked(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: unreadable UTF-8 source: {error}", path.display()))
}

/// Maps a declared corpus file extension to the syntax it is compiled as.
///
/// An unknown extension fails: the corpus may not declare a source the parser
/// test would silently skip.
fn script_kind_for(relative: &str) -> ScriptKind {
    if relative.ends_with(".d.ts") {
        return ScriptKind::TypeScript;
    }
    let extension = relative
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or_else(|| panic!("corpus source `{relative}` has no extension"));
    match extension {
        "ts" | "mts" | "cts" => ScriptKind::TypeScript,
        "tsx" => ScriptKind::TypeScriptReact,
        "js" | "mjs" | "cjs" => ScriptKind::JavaScript,
        "jsx" => ScriptKind::JavaScriptReact,
        "json" => ScriptKind::Json,
        other => panic!("corpus source `{relative}` has unsupported extension `{other}`"),
    }
}

/// Discovers every checked corpus source, deterministically ordered by path.
fn discover_corpus_sources() -> Vec<CorpusSource> {
    let root = repository_root();
    let manifest_path = root.join("corpus/manifest.toml");
    let manifest_text = read_checked(&manifest_path);
    let manifest = parse_minimal_toml(&manifest_path, &manifest_text);

    let manifest_root = manifest
        .first()
        .unwrap_or_else(|| unreachable!("the root table is always present"));
    assert!(
        manifest_root.text("schema").is_none(),
        "{}: `schema` must be a bare integer",
        manifest_path.display()
    );

    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    let mut project_ids: Vec<String> = Vec::new();

    let mut declare = |relative: &str, origin: String| {
        let path = root.join(relative);
        assert!(
            path.is_file(),
            "{origin} declares `{relative}`, which is not a checked file"
        );
        declared.entry(relative.to_owned()).or_insert(origin);
    };

    for table in manifest
        .iter()
        .filter(|table| table.header == "[[projects]]")
    {
        let id = table.required_text(&manifest_path, "id").to_owned();
        let spec_relative = table.required_text(&manifest_path, "spec").to_owned();
        let entrypoint = table.required_text(&manifest_path, "entrypoint").to_owned();
        declare(&entrypoint, format!("corpus manifest project `{id}`"));

        let spec_path = root.join(&spec_relative);
        assert!(
            spec_path.is_file(),
            "corpus manifest project `{id}` names missing spec `{spec_relative}`"
        );
        let spec_text = read_checked(&spec_path);
        let spec_tables = parse_minimal_toml(&spec_path, &spec_text);
        let spec = spec_tables
            .first()
            .unwrap_or_else(|| unreachable!("the root table is always present"));

        assert_eq!(
            spec.required_text(&spec_path, "id"),
            id,
            "spec `{spec_relative}` disagrees with the manifest project id"
        );
        assert_eq!(
            spec.required_text(&spec_path, "entrypoint"),
            entrypoint,
            "spec `{spec_relative}` disagrees with the manifest entrypoint"
        );
        let source_dir = spec.required_text(&spec_path, "source_dir");
        assert!(
            root.join(source_dir).is_dir(),
            "spec `{spec_relative}` names missing source dir `{source_dir}`"
        );

        let source_files = spec
            .list("source_files")
            .unwrap_or_else(|| panic!("spec `{spec_relative}` declares no `source_files` array"));
        assert!(
            !source_files.is_empty(),
            "spec `{spec_relative}` declares an empty `source_files` array"
        );
        for source_file in source_files {
            declare(source_file, format!("spec `{spec_relative}` source_files"));
        }

        project_ids.push(id);
    }

    assert!(
        !project_ids.is_empty(),
        "the corpus manifest declares no projects"
    );
    let mut sorted_ids = project_ids.clone();
    sorted_ids.sort();
    sorted_ids.dedup();
    assert_eq!(
        sorted_ids.len(),
        project_ids.len(),
        "the corpus manifest repeats a project id"
    );

    // Every spec on disk must be reachable from the manifest, so a project
    // cannot drop out of this sweep by being unlinked.
    let spec_directory = root.join("corpus/specs");
    let spec_count = fs::read_dir(&spec_directory)
        .unwrap_or_else(|error| panic!("{}: unreadable: {error}", spec_directory.display()))
        .filter(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "toml")
            })
        })
        .count();
    assert_eq!(
        spec_count,
        project_ids.len(),
        "corpus/specs holds {spec_count} specs but the manifest declares {} projects",
        project_ids.len()
    );

    declared
        .into_iter()
        .map(|(relative, origin)| CorpusSource {
            path: root.join(&relative),
            script_kind: script_kind_for(&relative),
            relative,
            origin,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Shebang
    )
}

/// Whether a token kind can be reinterpreted by an explicit parser rescan.
///
/// The default scanner pass resolves `/` as division and forms the longest `>`
/// operator, and segments templates itself; a grammar-aware parser may rescan
/// any of those. Streams free of these kinds cannot be reinterpreted, so the
/// parser's tokens must then equal the scanner's exactly.
fn is_rescannable(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Slash
            | TokenKind::SlashEq
            | TokenKind::GreaterThan
            | TokenKind::GreaterThanEq
            | TokenKind::GreaterGreater
            | TokenKind::GreaterGreaterEq
            | TokenKind::GreaterGreaterGreater
            | TokenKind::GreaterGreaterGreaterEq
            | TokenKind::TemplateHead
            | TokenKind::TemplateMiddle
            | TokenKind::TemplateTail
    )
}

/// Asserts that a token stream tiles its source exactly once, in order.
///
/// `lexeme` resolves a token against the stream's own owner, so a range that is
/// not a valid UTF-16 slice of that source is reported as a failure instead of
/// panicking inside the accessor.
fn assert_tiling(
    label: &str,
    tokens: &[Token],
    eof: &Token,
    source: &SourceText,
    lexeme: impl Fn(&Token) -> Option<String>,
) {
    let mut cursor = 0usize;
    let mut reconstructed = String::with_capacity(source.as_str().len());

    for (index, token) in tokens.iter().enumerate() {
        let range = token.range();
        assert_eq!(
            range.start().get(),
            cursor,
            "{label}: token {index} ({:?}) starts at {} but the previous token ended at {cursor}",
            token.kind(),
            range.start().get()
        );
        assert_ne!(
            token.kind(),
            TokenKind::EndOfFile,
            "{label}: token {index} is an end-of-file token inside the stream"
        );
        if token.is_missing() {
            assert!(
                range.is_empty(),
                "{label}: missing token {index} ({:?}) covers source text",
                token.kind()
            );
        } else {
            assert!(
                !range.is_empty(),
                "{label}: token {index} ({:?}) makes no forward progress",
                token.kind()
            );
        }

        let text = lexeme(token).unwrap_or_else(|| {
            panic!(
                "{label}: token {index} ({:?}) has range {}..{}, which is not a slice of its source",
                token.kind(),
                range.start().get(),
                range.end().get()
            )
        });
        assert_eq!(
            utf16_len(&text),
            range.len(),
            "{label}: token {index} ({:?}) lexeme length disagrees with its range",
            token.kind()
        );
        reconstructed.push_str(&text);
        cursor = range.end().get();
    }

    assert_eq!(
        eof.kind(),
        TokenKind::EndOfFile,
        "{label}: terminal token is {:?}, not end-of-file",
        eof.kind()
    );
    assert!(
        eof.range().is_empty(),
        "{label}: the end-of-file token covers source text"
    );
    assert_eq!(
        eof.range().start().get(),
        cursor,
        "{label}: the end-of-file token is not anchored at the end of the last token"
    );
    assert_eq!(
        cursor,
        source.len_utf16().get(),
        "{label}: the token stream stops before the end of the source"
    );
    assert_eq!(
        reconstructed,
        source.as_str(),
        "{label}: concatenated lexemes do not reproduce the source"
    );
}

/// Asserts diagnostics are canonically ordered and anchored inside the source.
fn assert_diagnostics_wellformed(
    label: &str,
    diagnostics: &[Diagnostic],
    source_id: SourceId,
    source: &SourceText,
) {
    assert!(
        diagnostics.is_sorted(),
        "{label}: diagnostics are not in canonical order: {diagnostics:?}"
    );
    for diagnostic in diagnostics {
        assert_eq!(
            diagnostic.source_id(),
            source_id,
            "{label}: diagnostic {diagnostic:?} is anchored in another source"
        );
        let range = diagnostic.range();
        assert!(
            source.utf16_to_byte(range.start()).is_ok()
                && source.utf16_to_byte(range.end()).is_ok(),
            "{label}: diagnostic {diagnostic:?} is not anchored at source boundaries"
        );
    }
}

/// Asserts every diagnostic of `subset` occurs, with multiplicity, in `whole`.
fn assert_contains_all(label: &str, whole: &[Diagnostic], subset: &[Diagnostic]) {
    let mut remaining = whole.iter();
    for diagnostic in subset {
        let found = remaining.any(|candidate| candidate == diagnostic);
        assert!(
            found,
            "{label}: scanner diagnostic {diagnostic:?} was dropped by the parser"
        );
    }
}

#[derive(Debug)]
struct ParseOutcome {
    scanner_diagnostics: usize,
    parse_diagnostics: usize,
    statement_kinds: Vec<NodeKind>,
    rescan_free: bool,
}

/// Scans and parses one source, asserting every property shared by all sources.
fn check_source(
    label: &str,
    source_id: SourceId,
    script_kind: ScriptKind,
    text: &str,
) -> ParseOutcome {
    let source = Arc::new(SourceText::new(text).expect("test source fits the per-file budget"));
    let scanned_recovered = scan(source_id, script_kind, Arc::clone(&source));
    let scanned = scanned_recovered.product().clone();
    let scanner_diagnostics = scanned_recovered.diagnostics().to_vec();

    assert!(
        Arc::ptr_eq(scanned.source(), &source),
        "{label}: the scanner copied the source text instead of sharing it"
    );
    assert_eq!(
        scanned.source_id(),
        source_id,
        "{label}: the scanner lost the source identity"
    );
    assert_eq!(
        scanned.script_kind(),
        script_kind,
        "{label}: the scanner lost the script kind"
    );
    assert_tiling(
        &format!("{label} (scanner)"),
        scanned.tokens(),
        scanned.eof(),
        scanned.source_text(),
        |token| scanned.token_text(token).map(str::to_owned),
    );
    for (index, token) in scanned.tokens().iter().enumerate() {
        assert!(
            !token.is_missing(),
            "{label}: the scanner produced a missing token at {index}"
        );
    }
    assert_diagnostics_wellformed(
        &format!("{label} (scanner)"),
        &scanner_diagnostics,
        source_id,
        &source,
    );

    let rescan_free = scanned
        .tokens()
        .iter()
        .all(|token| !is_rescannable(token.kind()))
        && !matches!(
            script_kind,
            ScriptKind::TypeScriptReact | ScriptKind::JavaScriptReact
        );

    let parsed = parse(scanned_recovered);
    let file = parsed.product();

    assert_eq!(
        file.source_id(),
        source_id,
        "{label}: the parser lost the source identity"
    );
    assert_eq!(
        file.script_kind(),
        script_kind,
        "{label}: the parser lost the script kind"
    );
    assert_eq!(
        file.source_text().as_str(),
        text,
        "{label}: the parser rewrote the source text"
    );
    assert_eq!(
        file.source_text().len_utf16(),
        source.len_utf16(),
        "{label}: the parser changed the source length"
    );
    assert_eq!(
        file.range().start().get(),
        0,
        "{label}: the source file does not start at the beginning of the source"
    );
    assert_eq!(
        file.range().end(),
        source.len_utf16(),
        "{label}: the source file does not span the whole source"
    );
    assert_eq!(
        file.eof(),
        scanned.eof(),
        "{label}: the parser did not preserve the scanner end-of-file token"
    );
    assert_tiling(
        &format!("{label} (parser)"),
        file.tokens(),
        file.eof(),
        file.source_text(),
        |token| file.token_text(token).map(str::to_owned),
    );

    // Trivia is preserved: a rescan may only *absorb* trivia (e.g. a regex
    // literal swallowing what the default pass read as a `//` comment), never
    // invent it, so the parser can carry no more trivia than the scanner did.
    let scanner_trivia = scanned
        .tokens()
        .iter()
        .filter(|token| is_trivia(token.kind()))
        .count();
    let parser_trivia = file
        .tokens()
        .iter()
        .filter(|token| is_trivia(token.kind()))
        .count();
    assert!(
        parser_trivia <= scanner_trivia,
        "{label}: the parser invented {} trivia token(s) the scanner never produced",
        parser_trivia - scanner_trivia
    );
    if rescan_free {
        // Nothing is rescannable, so the parser cannot reinterpret any code
        // token. Its only freedom is inserting empty-range recovery tokens, so
        // its non-missing tokens must reproduce the scanner stream exactly.
        let parser_real: Vec<Token> = file
            .tokens()
            .iter()
            .copied()
            .filter(|token| !token.is_missing())
            .collect();
        assert_eq!(
            parser_real.as_slice(),
            scanned.tokens(),
            "{label}: no token is rescannable, so the parser must keep the scanner stream"
        );
        assert_contains_all(label, parsed.diagnostics(), &scanner_diagnostics);
    }

    assert_eq!(
        file.diagnostics(),
        parsed.diagnostics(),
        "{label}: SourceFile diagnostics differ from the Recovered diagnostics"
    );
    assert_diagnostics_wellformed(
        &format!("{label} (parser)"),
        parsed.diagnostics(),
        source_id,
        &source,
    );

    let statement_kinds: Vec<NodeKind> = file
        .statements()
        .iter()
        .map(|statement| statement.kind())
        .collect();
    let has_code = scanned
        .tokens()
        .iter()
        .any(|token| !is_trivia(token.kind()));
    assert!(
        !has_code || !statement_kinds.is_empty(),
        "{label}: the source has code tokens but the parser produced no statement"
    );

    ParseOutcome {
        scanner_diagnostics: scanner_diagnostics.len(),
        parse_diagnostics: parsed.diagnostics().len(),
        statement_kinds,
        rescan_free,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn corpus_discovery_is_deterministic_and_complete() {
    let first = discover_corpus_sources();
    let second = discover_corpus_sources();

    assert_eq!(
        first, second,
        "corpus discovery is not deterministic across runs"
    );
    assert!(
        first.len() > 1,
        "corpus discovery found only {} source(s)",
        first.len()
    );

    let mut relatives: Vec<&str> = first
        .iter()
        .map(|source| source.relative.as_str())
        .collect();
    let sorted = {
        let mut sorted = relatives.clone();
        sorted.sort_unstable();
        sorted
    };
    assert_eq!(relatives, sorted, "corpus sources are not ordered by path");
    relatives.dedup();
    assert_eq!(
        relatives.len(),
        first.len(),
        "corpus discovery yielded a duplicate source"
    );

    // Every checked driver case is reachable from the manifest.
    let cases = repository_root().join("corpus/cases");
    for entry in fs::read_dir(&cases)
        .unwrap_or_else(|error| panic!("{}: unreadable: {error}", cases.display()))
    {
        let path = entry
            .unwrap_or_else(|error| panic!("{}: unreadable entry: {error}", cases.display()))
            .path();
        if !path.is_file() {
            continue;
        }
        let relative = format!(
            "corpus/cases/{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| panic!("{}: non-UTF-8 file name", path.display()))
        );
        assert!(
            first.iter().any(|source| source.relative == relative),
            "`{relative}` exists but is not declared by the corpus manifest"
        );
    }
}

#[test]
fn corpus_sources_scan_and_parse_totally() {
    let sources = discover_corpus_sources();
    let mut checked = 0usize;

    for (index, source) in sources.iter().enumerate() {
        let text = read_checked(&source.path);
        let source_id = SourceId::new(
            u32::try_from(index).unwrap_or_else(|_| panic!("corpus is larger than u32 source ids")),
        );
        let label = format!("{} (declared by {})", source.relative, source.origin);
        check_source(&label, source_id, source.script_kind, &text);
        checked += 1;
    }

    assert_eq!(
        checked,
        sources.len(),
        "not every discovered corpus source was checked"
    );
}

/// One focused, corpus-derived construct that must parse without diagnostics.
struct ConstructCase {
    name: &'static str,
    script_kind: ScriptKind,
    source: &'static str,
    statements: &'static [NodeKind],
}

/// Constructs taken from the checked corpus, written out so the exact syntax
/// under test is visible and zero diagnostics is a meaningful assertion.
const CONSTRUCTS: &[ConstructCase] = &[
    ConstructCase {
        // corpus/cases/mitt.ts, corpus/projects/rou3/src/index.ts
        name: "module-imports-and-type-only-specifiers",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "import mitt from \"../projects/mitt/src/index.ts\";\n",
            "import type { RouterContext } from \"./context.ts\";\n",
            "import { addRoute, type MatchedRoute } from \"./operations/add.ts\";\n",
            "import * as helpers from \"./_utils.ts\";\n",
            "export type { RouterContext };\n",
        ),
        statements: &[
            NodeKind::ImportDeclaration,
            NodeKind::ImportDeclaration,
            NodeKind::ImportDeclaration,
            NodeKind::ImportDeclaration,
            NodeKind::ExportDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/rou3/src/index.ts, corpus/projects/ohash/src/serialize.ts
        name: "export-forms",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export * from \"./types.ts\";\n",
            "export * as operations from \"./operations/find.ts\";\n",
            "export { addRoute as add, type RouterContext } from \"./context.ts\";\n",
            "export default function createRouter(): void {}\n",
        ),
        statements: &[
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/mitt/src/index.ts
        name: "generic-aliases-and-interface-overloads",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export type EventType = string | symbol;\n",
            "export type Handler<T = unknown> = (event: T) => void;\n",
            "export type EventHandlerMap<Events extends Record<EventType, unknown>> = Map<\n",
            "  keyof Events | \"*\",\n",
            "  Array<Handler<Events[keyof Events]>>\n",
            ">;\n",
            "export interface Emitter<Events extends Record<EventType, unknown>> {\n",
            "  all: EventHandlerMap<Events>;\n",
            "  on<Key extends keyof Events>(type: Key, handler: Handler<Events[Key]>): void;\n",
            "  on(type: \"*\", handler: (type: keyof Events) => void): void;\n",
            "  emit<Key extends keyof Events>(\n",
            "    type: undefined extends Events[Key] ? Key : never,\n",
            "  ): void;\n",
            "}\n",
        ),
        statements: &[
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
            NodeKind::ExportDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/mitt/src/index.ts default export
        name: "default-exported-generic-function",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export default function mitt<Events extends Record<string, unknown>>(\n",
            "  all?: Map<string, unknown>,\n",
            "): Map<string, unknown> {\n",
            "  type GenericHandler = ((event: unknown) => void) | undefined;\n",
            "  all = all || new Map();\n",
            "  const handler: GenericHandler = undefined;\n",
            "  return handler ? all : all;\n",
            "}\n",
        ),
        statements: &[NodeKind::ExportDeclaration],
    },
    ConstructCase {
        // corpus/projects/yocto-queue/index.js, corpus/projects/p-queue/source
        name: "class-private-members-and-generators",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export default class Queue<ValueType> implements Iterable<ValueType> {\n",
            "  #head: ValueType[] = [];\n",
            "  #size = 0;\n",
            "  #concurrency!: number;\n",
            "  static #instances = 0;\n",
            "  readonly label: string;\n",
            "  constructor(private readonly kind: string, label = \"queue\") {\n",
            "    this.label = label;\n",
            "  }\n",
            "  get size(): number {\n",
            "    return this.#size;\n",
            "  }\n",
            "  set size(value: number) {\n",
            "    this.#size = value;\n",
            "  }\n",
            "  #compact(): void {\n",
            "    this.#head = [];\n",
            "  }\n",
            "  clear(): void {\n",
            "    this.#compact();\n",
            "  }\n",
            "  *[Symbol.iterator](): Iterator<ValueType> {\n",
            "    for (const value of this.#head) {\n",
            "      yield value;\n",
            "    }\n",
            "  }\n",
            "  async *[Symbol.asyncIterator](): AsyncIterator<ValueType> {\n",
            "    yield await Promise.resolve(this.#head[0]);\n",
            "  }\n",
            "}\n",
        ),
        statements: &[NodeKind::ExportDeclaration],
    },
    ConstructCase {
        // corpus/projects/valita/src/index.ts
        name: "abstract-class-with-computed-members",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "const MATCHER_SYMBOL = Symbol(\"matcher\");\n",
            "abstract class AbstractType<Output = unknown> {\n",
            "  abstract readonly name: string;\n",
            "  abstract readonly [MATCHER_SYMBOL]: (value: unknown) => Output;\n",
            "  abstract optional<T>(defaultFn?: () => T): AbstractType<Output | T>;\n",
            "  protected constructor() {}\n",
            "}\n",
            "class Type<Output = unknown> extends AbstractType<Output> {\n",
            "  override readonly name = \"type\";\n",
            "  readonly [MATCHER_SYMBOL] = (value: unknown) => value as Output;\n",
            "  optional<T>(defaultFn?: () => T): AbstractType<Output | T> {\n",
            "    return this as unknown as AbstractType<Output | T>;\n",
            "  }\n",
            "}\n",
        ),
        statements: &[
            NodeKind::VariableDeclaration,
            NodeKind::ClassDeclaration,
            NodeKind::ClassDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/tslib, corpus/projects/destr type-level declarations
        name: "enum-namespace-and-ambient-declarations",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export const enum Mode {\n",
            "  Sync = 0,\n",
            "  Async = 1,\n",
            "}\n",
            "enum Level {\n",
            "  Low = \"low\",\n",
            "  High = \"high\",\n",
            "}\n",
            "namespace shims {\n",
            "  export const version = \"1\";\n",
            "}\n",
            "declare function nodeRequire(id: string): unknown;\n",
            "declare const globalScope: Record<string, unknown>;\n",
        ),
        statements: &[
            NodeKind::ExportDeclaration,
            NodeKind::EnumDeclaration,
            NodeKind::NamespaceDeclaration,
            NodeKind::DeclareStatement,
            NodeKind::DeclareStatement,
        ],
    },
    ConstructCase {
        // corpus/projects/valita/src/index.ts, corpus/projects/tiny-invariant
        name: "conditional-mapped-and-predicate-types",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "type PrettyIntersection<V> = Extract<{ [K in keyof V]: V[K] }, unknown>;\n",
            "type Mutable<T> = { -readonly [K in keyof T]-?: T[K] };\n",
            "type Optionalized<T> = { +readonly [K in keyof T as `get${string}`]+?: T[K] };\n",
            "type Element<T> = T extends ReadonlyArray<infer Item> ? Item : never;\n",
            "type Guard = (value: unknown) => asserts value is string;\n",
            "type Factory = new (value: string) => Date;\n",
            "type Query = typeof globalThis extends { console: infer C } ? C : never;\n",
            "type Indexed = Record<string, unknown>[\"key\"];\n",
            "type Tuple = readonly [head: string, ...rest: number[]];\n",
            "type Imported = import(\"./context.ts\").RouterContext<string>;\n",
        ),
        statements: &[
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/tiny-invariant/src/tiny-invariant.ts
        name: "assertion-signature-function",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export default function invariant(\n",
            "  condition: unknown,\n",
            "  message?: string | (() => string),\n",
            "): asserts condition {\n",
            "  if (condition) {\n",
            "    return;\n",
            "  }\n",
            "  const provided = typeof message === \"function\" ? message() : message;\n",
            "  throw new Error(provided ? `Invariant failed: ${provided}` : \"Invariant failed\");\n",
            "}\n",
        ),
        statements: &[NodeKind::ExportDeclaration],
    },
    ConstructCase {
        // corpus/projects/pathe/src/_glob.ts, corpus/projects/destr/src/index.ts
        name: "regular-expressions-and-division",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "const separator = /[/\\\\]+/g;\n",
            "const suspect = /^[\\s]*[\"[{]/;\n",
            "const escaped = \"a/b\".replace(/\\/+/gu, \"-\");\n",
            "const ratio = separator.source.length / escaped.length / 2;\n",
            "let counter = 10;\n",
            "counter /= 2;\n",
            "const grouped = /(?<name>[a-z]+)\\/(?<rest>.*)/su.exec(escaped)?.groups;\n",
        ),
        statements: &[
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExpressionStatement,
            NodeKind::VariableDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/mitt (nested type arguments), corpus/projects/ufo/src/punycode.ts
        name: "nested-type-arguments-and-shifts",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "type Handlers<T> = Map<string, Array<Array<T>>>;\n",
            "type Deep = Promise<Map<string, Set<Array<number>>>>;\n",
            "const mask = 0xff_ff >> 2;\n",
            "const unsigned = -1 >>> 24;\n",
            "const shifted = (1 << 3) >= 8 ? 1 : 0;\n",
            "let acc = 1;\n",
            "acc >>>= 1;\n",
            "acc >>= 1;\n",
            "const compared = acc > 0 && shifted >= 0;\n",
        ),
        statements: &[
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExpressionStatement,
            NodeKind::ExpressionStatement,
            NodeKind::VariableDeclaration,
        ],
    },
    ConstructCase {
        // corpus/cases/mitt.ts template logging, corpus/projects/rou3 regexp builders
        name: "templates-tagged-and-template-types",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "const label = `count: ${1 + 2} and ${`inner ${\"x\"} end`}`;\n",
            "const raw = String.raw`a\\nb${label}`;\n",
            "process.stdout.write(`${label}: ${JSON.stringify({ label })}\\n`);\n",
            "type Key = `on${Capitalize<string>}`;\n",
            "type Route = `/${string}/${number}`;\n",
        ),
        statements: &[
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExpressionStatement,
            NodeKind::TypeAliasDeclaration,
            NodeKind::TypeAliasDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/rou3/src/operations/add.ts, corpus/projects/dot-prop/index.js
        name: "optional-chaining-and-logical-assignment",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "declare const ctx: Record<string, any>;\n",
            "const dataMap = (ctx.dataMap ??= new Map());\n",
            "const value = ctx?.parent?.[\"key\"]?.value ?? dataMap;\n",
            "const called = ctx.handler?.(value)?.result;\n",
            "ctx.count ||= 0;\n",
            "ctx.flag &&= true;\n",
            "ctx.total ??= value ?? 0;\n",
        ),
        statements: &[
            NodeKind::DeclareStatement,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExpressionStatement,
            NodeKind::ExpressionStatement,
            NodeKind::ExpressionStatement,
        ],
    },
    ConstructCase {
        // corpus/projects/p-map/index.js, corpus/projects/perfect-debounce/src/index.ts
        name: "async-await-and-for-await-of",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "export async function run(items: AsyncIterable<number>): Promise<number> {\n",
            "  let total = 0;\n",
            "  for await (const item of items) {\n",
            "    total += await Promise.resolve(item);\n",
            "  }\n",
            "  const settled = await Promise.all([Promise.resolve(1)]);\n",
            "  return total + settled.length;\n",
            "}\n",
            "const debounced = async (): Promise<void> => {\n",
            "  await new Promise<void>((resolve) => {\n",
            "    resolve();\n",
            "  });\n",
            "};\n",
        ),
        statements: &[NodeKind::ExportDeclaration, NodeKind::VariableDeclaration],
    },
    ConstructCase {
        // corpus/projects/rou3/src/operations/find.ts, corpus/projects/destr/src/index.ts
        name: "control-flow-statements",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "declare const nodes: string[];\n",
            "for (let index = 0; index < nodes.length; index++) {\n",
            "  switch (index % 3) {\n",
            "    case 0: {\n",
            "      continue;\n",
            "    }\n",
            "    case 1:\n",
            "      break;\n",
            "    default:\n",
            "      ;\n",
            "  }\n",
            "}\n",
            "for (const node of nodes) {\n",
            "  if (node) {\n",
            "    break;\n",
            "  } else if (!node) {\n",
            "    continue;\n",
            "  }\n",
            "}\n",
            "for (const key in nodes) {\n",
            "  void key;\n",
            "}\n",
            "while (nodes.length > 0) {\n",
            "  nodes.pop();\n",
            "}\n",
            "try {\n",
            "  throw new Error(\"x\");\n",
            "} catch {\n",
            "  // ignored\n",
            "} finally {\n",
            "  nodes.length = 0;\n",
            "}\n",
            "try {\n",
            "  nodes.pop();\n",
            "} catch (error: unknown) {\n",
            "  throw error;\n",
            "}\n",
        ),
        statements: &[
            NodeKind::DeclareStatement,
            NodeKind::ForStatement,
            NodeKind::ForOfStatement,
            NodeKind::ForInStatement,
            NodeKind::WhileStatement,
            NodeKind::TryStatement,
            NodeKind::TryStatement,
        ],
    },
    ConstructCase {
        // corpus/projects/defu/src/_utils.ts, corpus/projects/citty/src/_parser.ts
        name: "destructuring-spread-and-defaults",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "declare const options: Record<string, any>;\n",
            "declare const values: number[];\n",
            "const { alias: renamed = \"x\", nested: { deep = false } = {}, ...rest } = options;\n",
            "const [first, , third = 3, ...others] = values;\n",
            "export function merge<T>(\n",
            "  { deep = false }: { deep?: boolean } = {},\n",
            "  ...sources: readonly T[]\n",
            "): T[] {\n",
            "  return [...sources, ...(deep ? sources : [])];\n",
            "}\n",
            "const merged = { ...options, extra: 1, [\"computed\"]: 2, renamed };\n",
            "[first, third] = [third, first];\n",
        ),
        statements: &[
            NodeKind::DeclareStatement,
            NodeKind::DeclareStatement,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExportDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::ExpressionStatement,
        ],
    },
    ConstructCase {
        // corpus/projects/rou3 (`satisfies`), corpus/projects/ufo, corpus/cases (`as const`)
        name: "assertions-satisfies-and-const-context",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "type Entry = { method: string; path: string };\n",
            "const entry = { method: \"GET\", path: \"/\" } satisfies Entry;\n",
            "const modes = [\"sync\", \"async\"] as const;\n",
            "const width = (entry as unknown as { width: number }).width!;\n",
            "const first = modes[0]!;\n",
            "const meta = new URL(\".\", import.meta.url);\n",
            "const dynamic = import(\"./context.ts\");\n",
        ),
        statements: &[
            NodeKind::TypeAliasDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/p-queue (BigInt ids), corpus/projects/ufo/src/punycode.ts
        name: "numeric-literal-forms",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "const id = 1n;\n",
            "const big = 0b1010_0001n;\n",
            "const hex = 0xdead_beef;\n",
            "const octal = 0o755;\n",
            "const float = 1_000.000_1e-3;\n",
            "const tiny = .5;\n",
            "const exponent = 2e10;\n",
        ),
        statements: &[
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
            NodeKind::VariableDeclaration,
        ],
    },
    ConstructCase {
        // corpus/projects/tslib/tslib.es6.mjs
        name: "javascript-prototype-idioms",
        script_kind: ScriptKind::JavaScript,
        source: concat!(
            "var extendStatics = function (d, b) {\n",
            "  extendStatics =\n",
            "    Object.setPrototypeOf ||\n",
            "    ({ __proto__: [] } instanceof Array &&\n",
            "      function (d, b) {\n",
            "        d.__proto__ = b;\n",
            "      }) ||\n",
            "    function (d, b) {\n",
            "      for (var p in b) if (Object.prototype.hasOwnProperty.call(b, p)) d[p] = b[p];\n",
            "    };\n",
            "  return extendStatics(d, b);\n",
            "};\n",
            "export function __extends(d, b) {\n",
            "  if (typeof b !== \"function\" && b !== null)\n",
            "    throw new TypeError(String(b) + \" is not a constructor or null\");\n",
            "  extendStatics(d, b);\n",
            "  function __() {\n",
            "    this.constructor = d;\n",
            "  }\n",
            "  d.prototype = b === null ? Object.create(b) : ((__.prototype = b.prototype), new __());\n",
            "}\n",
        ),
        statements: &[NodeKind::VariableDeclaration, NodeKind::ExportDeclaration],
    },
    ConstructCase {
        // corpus/projects/yocto-queue/index.js, corpus/projects/dot-prop/index.js
        name: "javascript-generators-and-classes",
        script_kind: ScriptKind::JavaScript,
        source: concat!(
            "export default class Queue {\n",
            "  #head;\n",
            "  #size = 0;\n",
            "  constructor() {\n",
            "    this.clear();\n",
            "  }\n",
            "  clear() {\n",
            "    this.#head = undefined;\n",
            "    this.#size = 0;\n",
            "  }\n",
            "  * [Symbol.iterator]() {\n",
            "    let current = this.#head;\n",
            "    while (current) {\n",
            "      yield current.value;\n",
            "      current = current.next;\n",
            "    }\n",
            "  }\n",
            "  async * drain() {\n",
            "    yield this.dequeue();\n",
            "  }\n",
            "}\n",
            "function* deepKeysIterator(object) {\n",
            "  for (const key of Object.keys(object)) {\n",
            "    yield key;\n",
            "  }\n",
            "}\n",
        ),
        statements: &[NodeKind::ExportDeclaration, NodeKind::FunctionDeclaration],
    },
    ConstructCase {
        // corpus/projects/destr/deno.ts
        name: "shebang-prefixed-module",
        script_kind: ScriptKind::TypeScript,
        source: concat!(
            "#!/usr/bin/env -S node --experimental-strip-types\n",
            "export { destr } from \"./src/index.ts\";\n",
        ),
        statements: &[NodeKind::ExportDeclaration],
    },
];

#[test]
fn focused_corpus_constructs_parse_without_diagnostics() {
    for (index, case) in CONSTRUCTS.iter().enumerate() {
        let source_id = SourceId::new(
            u32::try_from(index).unwrap_or_else(|_| unreachable!("the table is small")),
        );
        let outcome = check_source(case.name, source_id, case.script_kind, case.source);

        assert_eq!(
            outcome.parse_diagnostics, 0,
            "{}: expected a diagnostic-free parse",
            case.name
        );
        if outcome.rescan_free {
            assert_eq!(
                outcome.scanner_diagnostics, 0,
                "{}: no token is rescannable, so the scan must also be diagnostic-free",
                case.name
            );
        }
        assert_eq!(
            outcome.statement_kinds.as_slice(),
            case.statements,
            "{}: unexpected top-level statement shape",
            case.name
        );
    }
}
