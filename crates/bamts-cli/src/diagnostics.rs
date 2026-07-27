//! Deterministic rendering of compiler [`Diagnostic`] values.
//!
//! This module turns compiler diagnostics plus their [`SourceText`] into strings
//! for each [`DiagnosticsFormat`]. It never writes to stdout/stderr: callers own
//! the actual I/O so that rendering stays a pure, testable transformation.
//!
//! Why the design looks like this:
//! - **Determinism.** Diagnostics are sorted by the compiler's own total order
//!   (`source_id`, start, end, code, severity, message) before rendering, so the
//!   same set of diagnostics always produces byte-identical output regardless of
//!   the order the pipeline reported them in.
//! - **UTF-16 coordinates.** Reported line/column numbers are UTF-16 code-unit
//!   based (via [`SourceText::line_column`]), matching the coordinate space
//!   editors and the language server protocol use. A non-BMP character therefore
//!   advances the reported column by two.
//! - **Visual snippets.** The caret underline in `Pretty` output is aligned by
//!   UTF-16 code-unit count, consistent with the reported columns. This is not a
//!   perfect terminal-cell alignment (a wide glyph may occupy two cells while a
//!   BMP character occupies one), but it keeps the caret and the reported column
//!   in the same coordinate space and tracks wide non-BMP glyphs more closely
//!   than a code-point count would.
//! - **Hand-written JSON.** JSON is emitted without serde, with explicit escaping
//!   and a stable field order.

use std::fmt::Write as _;

use bamts_compiler::diagnostic::{Diagnostic, DiagnosticSeverity};
use bamts_compiler::source::{SourceId, SourceText, TextRange};

use crate::args::DiagnosticsFormat;

/// A named source available to the renderer for location and snippet resolution.
///
/// The `id` is matched against [`Diagnostic::source_id`]. A diagnostic whose
/// source is absent from the provided slice still renders, but without line,
/// column, or snippet information (its UTF-16 offsets are still reported).
#[derive(Clone, Copy)]
pub struct DiagnosticSource<'a> {
    /// The identifier the compiler stamped onto diagnostics for this source.
    pub id: SourceId,
    /// The display name (typically a path) shown in rendered locations.
    pub name: &'a str,
    /// The source text used to compute line/column and snippets.
    pub text: &'a SourceText,
}

/// Renders `diagnostics` in the requested `format`.
///
/// `sources` supplies the text and display name for each referenced
/// [`SourceId`]. The returned string carries a trailing newline per diagnostic
/// for the line-oriented formats and is empty when there are no diagnostics
/// (except `Json`, which always renders a well-formed array).
#[must_use]
pub fn render(
    format: DiagnosticsFormat,
    diagnostics: &[Diagnostic],
    sources: &[DiagnosticSource<'_>],
) -> String {
    let ordered = ordered(diagnostics);
    match format {
        DiagnosticsFormat::Text => render_text(&ordered, sources),
        DiagnosticsFormat::Pretty => render_pretty(&ordered, sources),
        DiagnosticsFormat::Json => render_json(&ordered, sources),
        DiagnosticsFormat::Github => render_github(&ordered, sources),
        DiagnosticsFormat::Compact => render_compact(&ordered, sources),
    }
}

/// Returns the diagnostics in the compiler's canonical total order.
fn ordered(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    let mut ordered: Vec<&Diagnostic> = diagnostics.iter().collect();
    ordered.sort();
    ordered
}

/// The stable lowercase name of a severity.
const fn severity_str(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
    }
}

/// Finds the source registered for `id`, if any.
fn find_source<'a>(
    sources: &'a [DiagnosticSource<'a>],
    id: SourceId,
) -> Option<&'a DiagnosticSource<'a>> {
    sources.iter().find(|source| source.id == id)
}

/// A resolved diagnostic anchor.
///
/// UTF-16 offsets are always available from the diagnostic range. Line/column
/// pairs (1-based, UTF-16) are present only when a source text is registered and
/// the range endpoints map to valid boundaries.
struct Location {
    name: String,
    start_offset: usize,
    end_offset: usize,
    /// `((start_line, start_col), (end_line, end_col))`, all 1-based UTF-16.
    line_col: Option<((usize, usize), (usize, usize))>,
}

/// Resolves the printable location for `diagnostic`.
fn locate(diagnostic: &Diagnostic, sources: &[DiagnosticSource<'_>]) -> Location {
    let range = diagnostic.range();
    let start_offset = range.start().get();
    let end_offset = range.end().get();
    let source = find_source(sources, diagnostic.source_id());
    let name = match source {
        Some(source) => source.name.to_owned(),
        None => format!("<source {}>", diagnostic.source_id().get()),
    };

    let line_col = source.and_then(|source| {
        let start = source.text.line_column(range.start()).ok()?;
        let end = source.text.line_column(range.end()).ok()?;
        Some(((start.0 + 1, start.1 + 1), (end.0 + 1, end.1 + 1)))
    });

    Location {
        name,
        start_offset,
        end_offset,
        line_col,
    }
}

/// Formats the `name:line:col` prefix, falling back to an offset when the source
/// text is unavailable.
fn location_prefix(location: &Location) -> String {
    match location.line_col {
        Some(((line, col), _)) => format!("{}:{}:{}", location.name, line, col),
        None => format!("{}:offset {}", location.name, location.start_offset),
    }
}

/// Renders the plain-text format: one `name:line:col: severity[code]: message`
/// line per diagnostic.
fn render_text(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        let location = locate(diagnostic, sources);
        let _ = writeln!(
            out,
            "{}: {}[{}]: {}",
            location_prefix(&location),
            severity_str(diagnostic.severity()),
            diagnostic.code().as_str(),
            diagnostic.message(),
        );
    }
    out
}

/// Renders the compact format: `name:line:col: level: message` with no code.
fn render_compact(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        let location = locate(diagnostic, sources);
        let _ = writeln!(
            out,
            "{}: {}: {}",
            location_prefix(&location),
            severity_str(diagnostic.severity()),
            diagnostic.message(),
        );
    }
    out
}

/// Renders the rich format with a source snippet and caret underline.
fn render_pretty(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        let location = locate(diagnostic, sources);
        let _ = writeln!(
            out,
            "{}[{}]: {}",
            severity_str(diagnostic.severity()),
            diagnostic.code().as_str(),
            diagnostic.message(),
        );

        let Some(((line, col), _)) = location.line_col else {
            let _ = writeln!(
                out,
                " --> {}:offset {}..{}",
                location.name, location.start_offset, location.end_offset,
            );
            continue;
        };

        let _ = writeln!(out, " --> {}:{}:{}", location.name, line, col);

        let source = find_source(sources, diagnostic.source_id());
        let snippet = source.and_then(|source| snippet(source.text, diagnostic.range()));
        match snippet {
            Some(snippet) => {
                let gutter = " ".repeat(snippet.line_number_width);
                let _ = writeln!(out, "{gutter} |");
                let _ = writeln!(out, "{} | {}", snippet.line_number, snippet.line_text);
                let _ = writeln!(
                    out,
                    "{gutter} | {}{}",
                    " ".repeat(snippet.caret_indent),
                    "^".repeat(snippet.caret_width),
                );
            }
            None => {
                let _ = writeln!(out, "  |");
            }
        }
    }
    out
}

/// A resolved single-line snippet with caret geometry measured in UTF-16 units.
struct Snippet {
    line_number: usize,
    line_number_width: usize,
    line_text: String,
    caret_indent: usize,
    caret_width: usize,
}

/// Builds a snippet for the line containing `range`'s start.
///
/// The caret indent and width use the same UTF-16 coordinate space as reported
/// columns, including two-unit non-BMP code points.
fn snippet(text: &SourceText, range: TextRange) -> Option<Snippet> {
    let (line_index, _) = text.line_column(range.start()).ok()?;
    let start_byte = text.utf16_to_byte(range.start()).ok()?;
    let end_byte = text.utf16_to_byte(range.end()).ok()?;

    let raw = text.as_str();
    let (line_start, line_end) = line_ranges(raw).into_iter().nth(line_index)?;

    // Clamp the underlined span to the snippet line.
    let span_start = start_byte.max(line_start);
    let span_end = end_byte.min(line_end).max(span_start);

    let line_start_utf16 = text.byte_to_utf16(line_start).ok()?.get();
    let span_start_utf16 = text.byte_to_utf16(span_start).ok()?.get();
    let span_end_utf16 = text.byte_to_utf16(span_end).ok()?.get();
    let caret_indent = span_start_utf16 - line_start_utf16;
    let caret_width = (span_end_utf16 - span_start_utf16).max(1);
    let line_number = line_index + 1;

    Some(Snippet {
        line_number,
        line_number_width: line_number.to_string().len(),
        line_text: raw.get(line_start..line_end)?.to_owned(),
        caret_indent,
        caret_width,
    })
}

/// Returns each line's `(content_start_byte, content_end_byte)`, excluding the
/// terminator, using the same line-break rules as [`SourceText`]: `\n`, bare
/// `\r`, `\r\n` (one break), `\u{2028}`, and `\u{2029}`.
fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut line_start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        match character {
            '\n' | '\u{2028}' | '\u{2029}' => {
                ranges.push((line_start, index));
                line_start = index + character.len_utf8();
            }
            '\r' => {
                ranges.push((line_start, index));
                if matches!(chars.peek(), Some(&(_, '\n'))) {
                    let (newline_index, newline) = chars.next().expect("peeked newline");
                    line_start = newline_index + newline.len_utf8();
                } else {
                    line_start = index + 1;
                }
            }
            _ => {}
        }
    }
    ranges.push((line_start, text.len()));
    ranges
}

/// Renders GitHub Actions workflow annotations
/// (`::error file=...,line=...,col=...::message`).
fn render_github(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        let location = locate(diagnostic, sources);
        let level = severity_str(diagnostic.severity());
        let _ = write!(out, "::{level} file={}", escape_github_property(&location.name));
        if let Some(((line, col), (end_line, end_col))) = location.line_col {
            let _ = write!(
                out,
                ",line={line},col={col},endLine={end_line},endColumn={end_col}",
            );
        }
        let _ = writeln!(out, "::{}", escape_github_data(diagnostic.message()));
    }
    out
}

/// Renders a hand-written JSON array of diagnostic objects with stable field
/// order. Line/column fields are `null` when the source text is unavailable.
fn render_json(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::from("[");
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let location = locate(diagnostic, sources);
        let (line, col, end_line, end_col) = match location.line_col {
            Some(((line, col), (end_line, end_col))) => (
                line.to_string(),
                col.to_string(),
                end_line.to_string(),
                end_col.to_string(),
            ),
            None => (
                "null".to_owned(),
                "null".to_owned(),
                "null".to_owned(),
                "null".to_owned(),
            ),
        };
        let _ = write!(
            out,
            concat!(
                "{{\"sourceId\":{},\"source\":\"{}\",",
                "\"severity\":\"{}\",",
                "\"code\":\"{}\",",
                "\"message\":\"{}\",",
                "\"startOffset\":{},",
                "\"endOffset\":{},",
                "\"line\":{},",
                "\"column\":{},",
                "\"endLine\":{},",
                "\"endColumn\":{}}}",
            ),
            diagnostic.source_id().get(),
            escape_json(&location.name),
            severity_str(diagnostic.severity()),
            escape_json(diagnostic.code().as_str()),
            escape_json(diagnostic.message()),
            location.start_offset,
            location.end_offset,
            line,
            col,
            end_line,
            end_col,
        );
    }
    out.push(']');
    out
}

/// Escapes a string for inclusion inside a JSON string literal.
///
/// Escapes `"`, `\`, all C0 control characters (`U+0000`–`U+001F`), and the
/// JavaScript line separators U+2028/U+2029. The short forms are used where JSON
/// defines them and `\uXXXX` otherwise. Other code points, including non-BMP
/// characters, pass through as valid UTF-8.
fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out
}

/// Escapes GitHub workflow-command message data (`%`, CR, LF).
fn escape_github_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escapes a GitHub workflow-command property value (data escapes plus `,` and
/// `:`).
fn escape_github_property(value: &str) -> String {
    escape_github_data(value)
        .replace(',', "%2C")
        .replace(':', "%3A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_compiler::diagnostic::DiagnosticCode;
    use bamts_compiler::source::Utf16Pos;

    const FILE: SourceId = SourceId::new(0);

    fn code(value: &'static str) -> DiagnosticCode {
        DiagnosticCode::new(value)
    }

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered range")
    }

    fn sources<'a>(name: &'a str, text: &'a SourceText) -> Vec<DiagnosticSource<'a>> {
        vec![DiagnosticSource {
            id: FILE,
            name,
            text,
        }]
    }

    #[test]
    fn text_reports_utf16_line_and_column() {
        let text = SourceText::new("let x = 1;\nlet y = 2;\n");
        let diag = Diagnostic::error(code("BTS0001"), FILE, range(15, 16), "bad y");
        let out = render(DiagnosticsFormat::Text, &[diag], &sources("main.ts", &text));
        // Offset 15 is line 2 (1-based), UTF-16 column 5 (1-based).
        assert_eq!(out, "main.ts:2:5: error[BTS0001]: bad y\n");
    }

    #[test]
    fn diagnostics_render_in_canonical_order() {
        let text = SourceText::new("aaaaaaaa");
        let later = Diagnostic::error(code("BTS0002"), FILE, range(4, 5), "later");
        let earlier = Diagnostic::warning(code("BTS0001"), FILE, range(1, 2), "earlier");
        // Supplied out of order; output must be sorted by position.
        let out = render(
            DiagnosticsFormat::Compact,
            &[later, earlier],
            &sources("a.ts", &text),
        );
        assert_eq!(
            out,
            "a.ts:1:2: warning: earlier\na.ts:1:5: error: later\n",
        );
    }

    #[test]
    fn json_has_stable_fields_and_escapes_strings() {
        let text = SourceText::new("x");
        let diag = Diagnostic::error(code("BTS0003"), FILE, range(0, 1), "a\"b\\c\n\td");
        let out = render(DiagnosticsFormat::Json, &[diag], &sources("q.ts", &text));
        assert_eq!(
            out,
            concat!(
                r#"[{"sourceId":0,"source":"q.ts","severity":"error","code":"BTS0003","#,
                r#""message":"a\"b\\c\n\td","startOffset":0,"endOffset":1,"#,
                r#""line":1,"column":1,"endLine":1,"endColumn":2}]"#,
            ),
        );
    }

    #[test]
    fn json_escaper_covers_controls_and_javascript_line_separators() {
        assert_eq!(
            escape_json("\"\\\0\u{8}\u{c}\n\r\t\u{1f}\u{2028}\u{2029}😀"),
            "\\\"\\\\\\u0000\\b\\f\\n\\r\\t\\u001f\\u2028\\u2029😀",
        );
    }

    #[test]
    fn json_reports_null_positions_for_unknown_source() {
        let diag = Diagnostic::error(code("BTS0004"), FILE, range(2, 5), "no source");
        let out = render(DiagnosticsFormat::Json, &[diag], &[]);
        assert!(out.contains("\"source\":\"<source 0>\""));
        assert!(out.contains("\"startOffset\":2,\"endOffset\":5"));
        assert!(out.contains("\"line\":null,\"column\":null"));
    }

    #[test]
    fn non_bmp_positions_and_pretty_carets_use_utf16_units() {
        // "😀" is two UTF-16 units; 'x' begins at UTF-16 position 2.
        let text = SourceText::new("😀x");
        let diag = Diagnostic::error(code("BTS0005"), FILE, range(2, 3), "on x");

        // JSON/text column is UTF-16 based: 'x' is at column 3 (1-based).
        let json = render(DiagnosticsFormat::Json, &[diag.clone()], &sources("e.ts", &text));
        assert!(
            json.contains("\"line\":1,\"column\":3,\"endLine\":1,\"endColumn\":4"),
            "utf16 columns wrong: {json}",
        );

        // Pretty uses the same UTF-16 coordinate: two columns precede 'x'.
        let pretty = render(DiagnosticsFormat::Pretty, &[diag], &sources("e.ts", &text));
        assert!(pretty.contains("1 | 😀x\n"), "snippet wrong: {pretty}");
        assert!(pretty.contains("  |   ^\n"), "caret misaligned: {pretty}");
    }

    #[test]
    fn pretty_underlines_multi_unit_range_within_line() {
        let text = SourceText::new("let value = 1;\n");
        let diag = Diagnostic::warning(code("BTS0006"), FILE, range(4, 9), "value");
        let out = render(DiagnosticsFormat::Pretty, &[diag], &sources("m.ts", &text));
        assert!(out.contains("warning[BTS0006]: value\n"));
        assert!(out.contains(" --> m.ts:1:5\n"));
        assert!(out.contains("1 | let value = 1;\n"));
        assert!(out.contains("  |     ^^^^^\n"), "carets wrong: {out}");
    }

    #[test]
    fn github_escapes_message_and_property_metacharacters() {
        let text = SourceText::new("x");
        let diag = Diagnostic::error(code("BTS0007"), FILE, range(0, 1), "bad, thing\nnext");
        let out = render(
            DiagnosticsFormat::Github,
            &[diag],
            &sources("dir,name:1.ts", &text),
        );
        assert!(out.starts_with("::error file=dir%2Cname%3A1.ts,line=1,col=1,endLine=1,endColumn=2::"));
        assert!(out.contains("bad, thing%0Anext"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn crlf_snippet_excludes_terminator() {
        let text = SourceText::new("a\r\nbc\r\n");
        // "bc" begins at UTF-16 offset 3 (after "a\r\n").
        let diag = Diagnostic::error(code("BTS0008"), FILE, range(3, 5), "bc");
        let out = render(DiagnosticsFormat::Pretty, &[diag], &sources("c.ts", &text));
        assert!(out.contains("2 | bc\n"), "crlf snippet wrong: {out}");
        assert!(out.contains("  | ^^\n"));
    }

    #[test]
    fn empty_diagnostics_render_predictably() {
        assert_eq!(render(DiagnosticsFormat::Text, &[], &[]), "");
        assert_eq!(render(DiagnosticsFormat::Pretty, &[], &[]), "");
        assert_eq!(render(DiagnosticsFormat::Compact, &[], &[]), "");
        assert_eq!(render(DiagnosticsFormat::Github, &[], &[]), "");
        assert_eq!(render(DiagnosticsFormat::Json, &[], &[]), "[]");
    }

    #[test]
    fn multiple_sources_group_by_source_id() {
        let a = SourceText::new("aa");
        let b = SourceText::new("bb");
        let src_a = SourceId::new(0);
        let src_b = SourceId::new(1);
        let da = Diagnostic::error(code("BTS0001"), src_a, range(0, 1), "in a");
        let db = Diagnostic::error(code("BTS0001"), src_b, range(0, 1), "in b");
        let catalog = vec![
            DiagnosticSource { id: src_a, name: "a.ts", text: &a },
            DiagnosticSource { id: src_b, name: "b.ts", text: &b },
        ];
        let out = render(DiagnosticsFormat::Compact, &[db, da], &catalog);
        assert_eq!(
            out,
            "a.ts:1:1: error: in a\nb.ts:1:1: error: in b\n",
        );
    }
}
