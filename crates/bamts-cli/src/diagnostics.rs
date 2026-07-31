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
//!   UTF-16 code-unit count, consistent with the reported columns. This is not
//!   a perfect terminal-cell alignment (a wide glyph may occupy two cells while
//!   a BMP character occupies one), but it keeps the caret and the reported
//!   column in the same coordinate space and tracks wide non-BMP glyphs more
//!   closely than a code-point count would. Source excerpts are capped to a
//!   fixed display width ([`PRETTY_SNIPPET_WIDTH`], 80 UTF-16 code units)
//!   centered on the diagnostic span, with `…` ellipses marking elided
//!   regions so output size stays bounded per diagnostic regardless of line
//!   length. Window edges are snapped to UTF-8 character boundaries so
//!   surrogate pairs are never split.
//! - **Hand-written JSON.** JSON is emitted without serde, with explicit escaping
//!   and a stable field order.

use std::fmt::Write as _;

use bamts_compiler::diagnostic::{Diagnostic, DiagnosticReport, DiagnosticSeverity};
use bamts_compiler::source::{SourceId, SourcePositionError, SourceText, TextRange, Utf16Pos};

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

/// Renders a compiler-prepared report without reimplementing its deduplication,
/// poisoning, per-rule cap, or summary accounting.
#[must_use]
pub fn render_report(
    format: DiagnosticsFormat,
    report: &DiagnosticReport,
    sources: &[DiagnosticSource<'_>],
    error_limit: usize,
) -> String {
    // Apply the compiler's canonical total order to the complete report, then
    // enforce the limit so the earliest canonical diagnostics are preserved
    // regardless of the order the pipeline reported them in.
    let diagnostics: Vec<Diagnostic> = ordered(report.diagnostics())
        .into_iter()
        .take(error_limit)
        .cloned()
        .collect();
    let mut rendered = render(format, &diagnostics, sources);
    if matches!(
        format,
        DiagnosticsFormat::Text | DiagnosticsFormat::Pretty | DiagnosticsFormat::Compact
    ) {
        for summary in report.summaries() {
            let _ = writeln!(
                rendered,
                "note[{}]: {} diagnostic(s); silence with `{}`",
                summary.rule().code(),
                summary.total_count(),
                summary.silence_flag(),
            );
        }
    }
    rendered
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
                let _ = writeln!(
                    out,
                    "{} | {}{}{}",
                    snippet.line_number,
                    snippet.prefix_ellipsis,
                    snippet.excerpt,
                    snippet.suffix_ellipsis,
                );
                let _ = writeln!(
                    out,
                    "{gutter} | {}{}{}",
                    snippet.prefix_ellipsis,
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

/// Maximum display width of a pretty source excerpt, measured in UTF-16 code
/// units. Lines longer than this are truncated to a window centered on the
/// diagnostic span, with `…` ellipses marking the elided regions. The cap is
/// in UTF-16 units (not bytes or terminal cells) so that the caret geometry
/// and the reported columns stay in the same coordinate space; a non-BMP
/// character contributes two units, matching [`SourceText::line_column`].
const PRETTY_SNIPPET_WIDTH: usize = 80;

/// The ellipsis string used to mark elided source regions.
const SNIPPET_ELLIPSIS: &str = "…";

/// A resolved single-line snippet with caret geometry measured in UTF-16 units.
///
/// `excerpt` is the possibly-truncated source text displayed to the user.
/// When the original line exceeds [`PRETTY_SNIPPET_WIDTH`], `prefix_ellipsis`
/// and/or `suffix_ellipsis` are set to `…` to mark the elided regions.
/// `caret_indent` is the offset from the start of `excerpt` (not including the
/// prefix ellipsis, which `render_pretty` prints separately), keeping the
/// caret aligned to the displayed text.
struct Snippet {
    line_number: usize,
    line_number_width: usize,
    excerpt: String,
    prefix_ellipsis: &'static str,
    suffix_ellipsis: &'static str,
    caret_indent: usize,
    caret_width: usize,
}

/// Builds a snippet for the line containing `range`'s start.
///
/// The caret indent and width use the same UTF-16 coordinate space as reported
/// columns, including two-unit non-BMP code points. When the line is longer
/// than [`PRETTY_SNIPPET_WIDTH`] UTF-16 units, the excerpt is truncated to a
/// window centered on the diagnostic span. Window edges are snapped to UTF-8
/// character boundaries so that surrogate pairs are never split.
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
    let line_end_utf16 = text.byte_to_utf16(line_end).ok()?.get();
    let span_start_utf16 = text.byte_to_utf16(span_start).ok()?.get();
    let span_end_utf16 = text.byte_to_utf16(span_end).ok()?.get();

    let line_len_utf16 = line_end_utf16 - line_start_utf16;
    let span_indent_utf16 = span_start_utf16 - line_start_utf16;
    let span_width_utf16 = (span_end_utf16 - span_start_utf16).max(1);
    let line_number = line_index + 1;

    // Fast path: the full line fits within the display budget.
    if line_len_utf16 <= PRETTY_SNIPPET_WIDTH {
        return Some(Snippet {
            line_number,
            line_number_width: line_number.to_string().len(),
            excerpt: raw.get(line_start..line_end)?.to_owned(),
            prefix_ellipsis: "",
            suffix_ellipsis: "",
            caret_indent: span_indent_utf16,
            caret_width: span_width_utf16,
        });
    }

    // Truncation path: center a window of PRETTY_SNIPPET_WIDTH UTF-16 units on
    // the span. Account for ellipsis width in the budget.
    let ellipsis_width = SNIPPET_ELLIPSIS.encode_utf16().count(); // 1
    let budget = PRETTY_SNIPPET_WIDTH - 2 * ellipsis_width;

    // Center the window on the span midpoint. If the span itself is wider than
    // the budget, left-align the window to the span start so the caret origin
    // is always visible.
    let visible_span = span_width_utf16.min(budget);
    let span_mid = span_indent_utf16 + visible_span / 2;
    let window_start_rel = span_mid.saturating_sub(budget / 2);
    let window_end_rel = (window_start_rel + budget).min(line_len_utf16);
    // Re-adjust start if end hit the line boundary (reclaim unused space).
    let window_start_rel = window_start_rel.min(window_end_rel.saturating_sub(budget));

    let window_start_utf16 = line_start_utf16 + window_start_rel;
    let window_end_utf16 = line_start_utf16 + window_end_rel;

    // Snap window edges to UTF-8 character boundaries. A UTF-16 offset landing
    // inside a surrogate pair (non-BMP char = 2 units) would cause
    // `utf16_to_byte` to error; nudge inward until we hit a valid boundary.
    let window_start_byte = snap_utf16_to_char_boundary(text, window_start_utf16, true)?;
    let window_end_byte = snap_utf16_to_char_boundary(text, window_end_utf16, false)?;

    // Recompute the actual window edges in UTF-16 after snapping.
    let snapped_start_utf16 = text.byte_to_utf16(window_start_byte).ok()?.get();
    let snapped_end_utf16 = text.byte_to_utf16(window_end_byte).ok()?.get();

    // Caret position relative to the displayed excerpt text (excluding the
    // prefix ellipsis, which `render_pretty` prints separately before the
    // indent spaces).
    let span_visible_start = span_start_utf16.max(snapped_start_utf16);
    let span_visible_end = span_end_utf16
        .min(snapped_end_utf16)
        .max(span_visible_start);
    let caret_indent = span_visible_start - snapped_start_utf16;
    let caret_width = (span_visible_end - span_visible_start).max(1);

    // Determine which ellipses are needed. Both sides are elided when the
    // window doesn't reach the respective line boundary.
    let need_prefix = window_start_rel > 0;
    let need_suffix = window_end_rel < line_len_utf16;
    let prefix_ellipsis: &'static str = if need_prefix { SNIPPET_ELLIPSIS } else { "" };
    let suffix_ellipsis: &'static str = if need_suffix { SNIPPET_ELLIPSIS } else { "" };

    Some(Snippet {
        line_number,
        line_number_width: line_number.to_string().len(),
        excerpt: raw.get(window_start_byte..window_end_byte)?.to_owned(),
        prefix_ellipsis,
        suffix_ellipsis,
        caret_indent,
        caret_width,
    })
}

/// Snaps a UTF-16 offset to a valid UTF-8 character boundary.
///
/// If `position` lands inside a surrogate pair (i.e. the corresponding byte
/// is a UTF-8 continuation byte), nudges one UTF-16 unit inward (`forward =
/// true` nudges toward higher offsets, `false` toward lower). Returns the
/// byte offset of the nearest valid character boundary.
fn snap_utf16_to_char_boundary(text: &SourceText, position: usize, forward: bool) -> Option<usize> {
    let raw = text.as_str();
    let mut pos = position;
    loop {
        match text.utf16_to_byte(Utf16Pos::new(pos)) {
            Ok(byte) => {
                debug_assert!(raw.is_char_boundary(byte));
                return Some(byte);
            }
            Err(SourcePositionError::Utf16PositionInsideSurrogatePair { .. }) => {
                pos = if forward {
                    pos.saturating_add(1)
                } else {
                    pos.saturating_sub(1)
                };
            }
            Err(_) => return None,
        }
    }
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
        let _ = write!(
            out,
            "::{level} file={}",
            escape_github_property(&location.name)
        );
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
    use bamts_compiler::lint::LintLevel;
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
        assert_eq!(out, "a.ts:1:2: warning: earlier\na.ts:1:5: error: later\n",);
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
        let json = render(
            DiagnosticsFormat::Json,
            std::slice::from_ref(&diag),
            &sources("e.ts", &text),
        );
        assert!(
            json.contains("\"line\":1,\"column\":3,\"endLine\":1,\"endColumn\":4"),
            "utf16 columns wrong: {json}",
        );

        // Pretty uses the same UTF-16 coordinate: two columns precede 'x'.
        let pretty = render(
            DiagnosticsFormat::Pretty,
            std::slice::from_ref(&diag),
            &sources("e.ts", &text),
        );
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
        assert!(
            out.starts_with("::error file=dir%2Cname%3A1.ts,line=1,col=1,endLine=1,endColumn=2::")
        );
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
            DiagnosticSource {
                id: src_a,
                name: "a.ts",
                text: &a,
            },
            DiagnosticSource {
                id: src_b,
                name: "b.ts",
                text: &b,
            },
        ];
        let out = render(DiagnosticsFormat::Compact, &[db, da], &catalog);
        assert_eq!(out, "a.ts:1:1: error: in a\nb.ts:1:1: error: in b\n",);
    }
    #[test]
    fn pretty_truncates_long_line_with_ellipses_and_aligned_caret() {
        // 120 ASCII chars; far exceeds PRETTY_SNIPPET_WIDTH (80).
        // The span targets 'X' at UTF-16 offset 60 (column 61, 1-based).
        let body = "a".repeat(60) + "X" + &"b".repeat(59);
        let text = SourceText::new(body.as_str());
        let diag = Diagnostic::error(code("BTS0009"), FILE, range(60, 61), "on X");
        let out = render(
            DiagnosticsFormat::Pretty,
            &[diag],
            &sources("long.ts", &text),
        );

        // Reported location is the true UTF-16 column, unaffected by truncation.
        assert!(out.contains(" --> long.ts:1:61\n"), "location wrong: {out}");

        // The source line is capped: prefix ellipsis + ≤78 chars + suffix ellipsis.
        let source_line = out
            .lines()
            .find(|line| line.starts_with("1 | "))
            .expect("source line");
        let displayed = source_line.strip_prefix("1 | ").expect("stripped");
        // … + excerpt + … ; excerpt is ≤78 UTF-16 units, total ≤80.
        assert!(
            displayed.starts_with('…'),
            "missing prefix ellipsis: {displayed}"
        );
        assert!(
            displayed.ends_with('…'),
            "missing suffix ellipsis: {displayed}"
        );
        let total_utf16 = displayed.encode_utf16().count();
        assert!(
            total_utf16 <= PRETTY_SNIPPET_WIDTH,
            "excerpt {total_utf16} exceeds cap {}: {displayed}",
            PRETTY_SNIPPET_WIDTH,
        );

        // The caret must point at 'X'. In the window, 'X' at offset 60 is
        // centered. The caret line has a leading ellipsis then spaces then '^'.
        let caret_line = out
            .lines()
            .find(|line| line.starts_with("  | "))
            .expect("caret line");
        let caret_displayed = caret_line.strip_prefix("  | ").expect("caret stripped");
        assert!(
            caret_displayed.starts_with('…'),
            "caret missing prefix ellipsis: {caret_displayed}"
        );
        // There is exactly one caret and it sits on 'X'.
        let carets = caret_displayed.matches('^').count();
        assert_eq!(carets, 1, "expected exactly one caret: {caret_displayed}");
        // The caret column (counting the leading ellipsis as 1) must match
        // the column of 'X' within the source line's displayed content.
        let caret_col = caret_displayed.find('^').expect("caret position");
        let x_col = displayed.find('X').expect("X in source line");
        assert_eq!(
            caret_col, x_col,
            "caret misaligned: caret={caret_displayed} source={displayed}"
        );
    }

    #[test]
    fn pretty_truncation_snaps_non_bmp_at_window_boundary() {
        // 77 ASCII chars + 😀 (2 UTF-16 units, 4 UTF-8 bytes) + 3 ASCII = 82
        // UTF-16 units total. With the span at offset 0, the window is
        // [0, 78). UTF-16 offset 78 lands on the low surrogate of 😀 — inside
        // the pair. The snap step must nudge backward to 77 so the excerpt
        // never splits the non-BMP character.
        let pre = "a".repeat(77);
        let mid = "😀"; // offsets 77..79 in UTF-16
        let post = "b".repeat(3);
        let body = format!("{pre}{mid}{post}");
        let text = SourceText::new(body.as_str());
        let diag = Diagnostic::error(code("BTS0010"), FILE, range(0, 1), "first");
        let out = render(DiagnosticsFormat::Pretty, &[diag], &sources("nb.ts", &text));

        // Must not panic or return None — the snap step handled the pair.
        let source_line = out
            .lines()
            .find(|line| line.starts_with("1 | "))
            .expect("source line");
        let displayed = source_line.strip_prefix("1 | ").expect("stripped");
        let total_utf16 = displayed.encode_utf16().count();
        assert!(
            total_utf16 <= PRETTY_SNIPPET_WIDTH,
            "non-bmp excerpt {total_utf16} exceeds cap: {displayed}",
        );
        // The non-BMP char must not be split — no replacement char from a
        // broken surrogate, and the output must remain valid UTF-8 (which
        // guarantees no lone surrogates survived into the string).
        assert!(
            !displayed.contains('\u{FFFD}'),
            "replacement char from split surrogate: {displayed}",
        );
        assert!(
            std::str::from_utf8(displayed.as_bytes()).is_ok(),
            "excerpt is not valid UTF-8: {displayed:?}",
        );

        // Caret line is well-formed and within bounds.
        let caret_line = out
            .lines()
            .find(|line| line.starts_with("  | "))
            .expect("caret line");
        assert!(caret_line.contains('^'), "no caret: {caret_line}");
    }

    #[test]
    fn snap_utf16_to_char_boundary_handles_surrogate_pair() {
        // "a😀b": 'a' at offset 0, 😀 at offsets 1..3 (2 UTF-16 units), 'b' at 3.
        let text = SourceText::new("a😀b");

        // Offset 2 lands on the low surrogate of 😀 (inside the pair).
        // Snapping backward (forward=false) → offset 1 (the high surrogate,
        // which is a valid char boundary).
        let snapped =
            snap_utf16_to_char_boundary(&text, 2, false).expect("snap backward from low surrogate");
        assert_eq!(
            text.as_str()[snapped..].chars().next(),
            Some('😀'),
            "backward snap should land on the 😀 start",
        );

        // Snapping forward (forward=true) from offset 1 (high surrogate) →
        // offset 3 ('b'), which is the next valid boundary after the pair.
        // But offset 1 itself IS valid (char start), so forward stays at 1.
        // Instead, snap forward from offset 2 (low surrogate): → offset 3.
        let snapped_fwd =
            snap_utf16_to_char_boundary(&text, 2, true).expect("snap forward from low surrogate");
        assert_eq!(
            text.as_str()[snapped_fwd..].chars().next(),
            Some('b'),
            "forward snap should land past the 😀",
        );

        // A valid offset passes through unchanged.
        let valid =
            snap_utf16_to_char_boundary(&text, 0, true).expect("valid offset snaps to itself");
        assert_eq!(valid, 0);
    }

    #[test]
    fn pretty_truncation_clamps_wide_span_to_window() {
        // A span wider than the display budget. The caret must be clamped to
        // the visible window, never underflow.
        let body = "a".repeat(200);
        let text = SourceText::new(body.as_str());
        // Span covers UTF-16 offsets 10..190 — far wider than the cap.
        let diag = Diagnostic::error(code("BTS0011"), FILE, range(10, 190), "wide");
        let out = render(
            DiagnosticsFormat::Pretty,
            &[diag],
            &sources("wide.ts", &text),
        );

        let source_line = out
            .lines()
            .find(|line| line.starts_with("1 | "))
            .expect("source line");
        let displayed = source_line.strip_prefix("1 | ").expect("stripped");
        let total_utf16 = displayed.encode_utf16().count();
        assert!(
            total_utf16 <= PRETTY_SNIPPET_WIDTH,
            "wide-span excerpt {total_utf16} exceeds cap: {displayed}",
        );

        // The caret width must be bounded by the visible portion.
        let caret_line = out
            .lines()
            .find(|line| line.starts_with("  | "))
            .expect("caret line");
        let caret_displayed = caret_line.strip_prefix("  | ").expect("caret stripped");
        let caret_width = caret_displayed.matches('^').count();
        assert!(
            caret_width <= PRETTY_SNIPPET_WIDTH,
            "caret width {caret_width} exceeds cap: {caret_displayed}",
        );
        assert!(caret_width >= 1, "wide span produced zero-width caret");
    }

    #[test]
    fn report_rendering_uses_compiler_summary_and_limit() {
        let text = SourceText::new("xx");
        let rule = bamts_compiler::lint::rule_by_name("explicit-any")
            .expect("registered rule")
            .id();
        let diagnostics = [
            Diagnostic::lint(LintLevel::Warn, rule, FILE, range(0, 1), "first").expect("warns"),
            Diagnostic::lint(LintLevel::Warn, rule, FILE, range(1, 2), "second").expect("warns"),
        ];
        let report = DiagnosticReport::new(&diagnostics);
        let out = render_report(DiagnosticsFormat::Text, &report, &sources("a.ts", &text), 1);
        assert!(out.contains("first"));
        assert!(!out.contains("second"));
        assert!(out.contains("2 diagnostic(s); silence with `-A explicit-any`"));
    }

    #[test]
    fn report_limit_preserves_earliest_canonical_diagnostics_across_modules() {
        // Regression: the limit must be applied after canonical ordering of the
        // full report, not to the raw pipeline order, so earlier diagnostics are
        // never hidden behind later ones when aggregating across modules.
        let text_a = SourceText::new("ab");
        let text_b = SourceText::new("xy");
        let src_a = SourceId::new(0);
        let src_b = SourceId::new(1);
        let catalog = vec![
            DiagnosticSource {
                id: src_a,
                name: "a.ts",
                text: &text_a,
            },
            DiagnosticSource {
                id: src_b,
                name: "b.ts",
                text: &text_b,
            },
        ];
        let diagnostics = [
            Diagnostic::error(code("BTS0002"), src_b, range(0, 1), "b-first"),
            Diagnostic::error(code("BTS0001"), src_a, range(1, 2), "a-second"),
            Diagnostic::error(code("BTS0003"), src_b, range(1, 2), "b-last"),
            Diagnostic::error(code("BTS0004"), src_a, range(0, 1), "a-earliest"),
        ];
        let report = DiagnosticReport::new(&diagnostics);
        let out = render_report(DiagnosticsFormat::Text, &report, &catalog, 3);

        let pos_earliest = out.find("a-earliest").expect("earliest A diagnostic");
        let pos_second = out.find("a-second").expect("second A diagnostic");
        let pos_b_first = out.find("b-first").expect("first B diagnostic");
        assert!(pos_earliest < pos_second, "A diagnostics out of order");
        assert!(
            pos_second < pos_b_first,
            "B diagnostic rendered before A diagnostics"
        );
        assert!(
            !out.contains("b-last"),
            "limit retained a later diagnostic over an earlier one"
        );
    }
}
