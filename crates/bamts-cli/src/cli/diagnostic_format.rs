//! TypeScript 7.0.2 diagnostic rendering: `--pretty false` canonical text and typed JSON.
//!
//! Rendering is a pure transformation. Callers own I/O. Every diagnostic is
//! emitted: this module never applies a silent error-limit cap.

use std::fmt::Write as _;

use bamts_compiler::diagnostic::{Diagnostic, DiagnosticReport, DiagnosticSeverity};
use bamts_compiler::source::SourceId;
#[cfg(test)]
use bamts_compiler::source::SourceText;

use crate::args::DiagnosticsFormat;
use crate::cli::tsc_args::TscExitStatus;
use crate::diagnostics::DiagnosticSource;

/// TypeScript 7.0.2 diagnostic presentation modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TscDiagnosticFormat {
    /// Canonical `tsc --pretty false` text: `file(line,col): error TSnnnn: message`.
    PrettyFalse,
    /// Color/context pretty (TTY default in TypeScript 7.0.2).
    Pretty,
    /// Typed JSON array. Field order is stable; no serde.
    Json,
}

impl TscDiagnosticFormat {
    /// Maps the existing driver format enum onto the TypeScript 7.0.2 surface.
    /// `Text` is canonical pretty-false; `Json` is typed JSON.
    #[must_use]
    pub const fn from_cli(format: DiagnosticsFormat) -> Self {
        match format {
            DiagnosticsFormat::Pretty => Self::Pretty,
            DiagnosticsFormat::Json => Self::Json,
            DiagnosticsFormat::Text | DiagnosticsFormat::Github | DiagnosticsFormat::Compact => {
                Self::PrettyFalse
            }
        }
    }
}

/// Maps [`DiagnosticsFormat`] onto the TypeScript 7.0.2 renderer.
#[must_use]
pub const fn format_from_cli(format: DiagnosticsFormat) -> TscDiagnosticFormat {
    TscDiagnosticFormat::from_cli(format)
}

/// Renders every diagnostic. Unlike [`crate::diagnostics::render_report`], this
/// never takes an error-limit and never drops a diagnostic.
#[must_use]
pub fn render(
    format: TscDiagnosticFormat,
    diagnostics: &[Diagnostic],
    sources: &[DiagnosticSource<'_>],
) -> String {
    let ordered = ordered(diagnostics);
    match format {
        TscDiagnosticFormat::PrettyFalse => render_pretty_false(&ordered, sources),
        TscDiagnosticFormat::Pretty => render_pretty(&ordered, sources),
        TscDiagnosticFormat::Json => render_json(&ordered, sources),
    }
}

/// Renders a compiler report without the historical 50-diagnostic CLI cap.
///
/// Does not rebuild the report (which would re-apply the per-rule cap). Notes
/// from lint summaries are omitted so `--pretty false` stays TypeScript-canonical.
#[must_use]
pub fn render_report(
    format: TscDiagnosticFormat,
    report: &DiagnosticReport,
    sources: &[DiagnosticSource<'_>],
) -> String {
    render(format, report.diagnostics(), sources)
}

/// TypeScript 7.0.2 exit status for a rendered diagnostic set.
#[must_use]
pub fn exit_status(diagnostics: &[Diagnostic], outputs_generated: bool) -> TscExitStatus {
    let has_errors = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity() == DiagnosticSeverity::Error);
    TscExitStatus::from_compilation(has_errors, outputs_generated)
}

/// Driver-facing name for [`exit_status`].
#[must_use]
pub fn exit_status_from_diagnostics(
    diagnostics: &[Diagnostic],
    outputs_generated: bool,
) -> TscExitStatus {
    exit_status(diagnostics, outputs_generated)
}

fn ordered(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    let mut ordered: Vec<&Diagnostic> = diagnostics.iter().collect();
    ordered.sort();
    ordered
}

fn find_source<'a>(
    sources: &'a [DiagnosticSource<'a>],
    id: SourceId,
) -> Option<&'a DiagnosticSource<'a>> {
    sources.iter().find(|source| source.id == id)
}

struct Location {
    name: String,
    start_offset: usize,
    end_offset: usize,
    line_col: Option<((usize, usize), (usize, usize))>,
}

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

fn category_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
    }
}

/// TypeScript prints `error TS2322:`; BAMTS catalog codes keep their identity.
fn code_label(code: &str) -> String {
    if code.chars().all(|ch| ch.is_ascii_digit()) {
        format!("TS{code}")
    } else {
        code.to_owned()
    }
}

fn write_pretty_false_line(
    out: &mut String,
    diagnostic: &Diagnostic,
    sources: &[DiagnosticSource<'_>],
) {
    let location = locate(diagnostic, sources);
    if find_source(sources, diagnostic.source_id()).is_some() {
        if let Some(((line, col), _)) = location.line_col {
            let _ = write!(out, "{}({},{}): ", location.name, line, col);
        } else {
            let _ = write!(out, "{}: ", location.name);
        }
    }
    let _ = writeln!(
        out,
        "{} {}: {}",
        category_name(diagnostic.severity()),
        code_label(diagnostic.code().as_str()),
        diagnostic.message(),
    );
}

fn render_pretty_false(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        write_pretty_false_line(&mut out, diagnostic, sources);
    }
    out
}

const GREY: &str = "\u{001b}[90m";
const RED: &str = "\u{001b}[91m";
const YELLOW: &str = "\u{001b}[93m";
const CYAN: &str = "\u{001b}[96m";
const RESET: &str = "\u{001b}[0m";

fn category_color(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => RED,
        DiagnosticSeverity::Warning => YELLOW,
    }
}

fn render_pretty(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::new();
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let location = locate(diagnostic, sources);
        if let Some(((line, col), _)) = location.line_col {
            let _ = write!(
                out,
                "{CYAN}{}{RESET}:{YELLOW}{line}{RESET}:{YELLOW}{col}{RESET} - ",
                location.name
            );
        }
        let _ = write!(
            out,
            "{color}{category}{RESET}{GREY} {code}: {RESET}{message}",
            color = category_color(diagnostic.severity()),
            category = category_name(diagnostic.severity()),
            code = code_label(diagnostic.code().as_str()),
            message = diagnostic.message(),
        );
        for related in diagnostic.secondary_spans() {
            if let Some(source) = find_source(sources, related.source_id())
                && let Ok((line, col)) = source.text.line_column(related.range().start())
            {
                let _ = write!(
                    out,
                    "\n  {CYAN}{}{RESET}:{YELLOW}{}{RESET}:{YELLOW}{}{RESET} - {}",
                    source.name,
                    line + 1,
                    col + 1,
                    related.label()
                );
            }
        }
    }
    out
}

fn render_json(diagnostics: &[&Diagnostic], sources: &[DiagnosticSource<'_>]) -> String {
    let mut out = String::from("[");
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_json_diagnostic(&mut out, diagnostic, sources);
    }
    out.push(']');
    out
}

fn write_json_diagnostic(
    out: &mut String,
    diagnostic: &Diagnostic,
    sources: &[DiagnosticSource<'_>],
) {
    let location = locate(diagnostic, sources);
    let has_file = find_source(sources, diagnostic.source_id()).is_some();
    out.push('{');
    let _ = write!(
        out,
        "\"code\":\"{}\",\"category\":\"{}\",\"message\":\"{}\"",
        escape_json(diagnostic.code().as_str()),
        category_name(diagnostic.severity()),
        escape_json(diagnostic.message()),
    );
    if has_file {
        let _ = write!(
            out,
            ",\"file\":\"{}\",\"span\":{{\"start\":{},\"end\":{}}}",
            escape_json(&location.name),
            location.start_offset,
            location.end_offset,
        );
        if let Some(((line, col), (end_line, end_col))) = location.line_col {
            let _ = write!(
                out,
                ",\"start\":{{\"line\":{line},\"character\":{col}}},\"end\":{{\"line\":{end_line},\"character\":{end_col}}}"
            );
        }
    }
    out.push_str(",\"related\":[");
    let mut related_index = 0usize;
    for related in diagnostic.secondary_spans() {
        if related_index > 0 {
            out.push(',');
        }
        related_index += 1;
        let _ = write!(out, "{{\"message\":\"{}\"", escape_json(related.label()));
        if let Some(source) = find_source(sources, related.source_id()) {
            let start = related.range().start().get();
            let end = related.range().end().get();
            let _ = write!(
                out,
                ",\"file\":\"{}\",\"span\":{{\"start\":{start},\"end\":{end}}}",
                escape_json(source.name),
            );
        }
        out.push('}');
    }
    for extra in [diagnostic.note(), diagnostic.help()].into_iter().flatten() {
        if related_index > 0 {
            out.push(',');
        }
        related_index += 1;
        let _ = write!(out, "{{\"message\":\"{}\"}}", escape_json(extra));
    }
    out.push(']');
    out.push('}');
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_compiler::diagnostic::{DiagnosticCode, SecondarySpan};
    use bamts_compiler::source::{TextRange, Utf16Pos};

    const FILE: SourceId = SourceId::new(0);
    const OTHER: SourceId = SourceId::new(1);

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
    fn pretty_false_matches_typescript_canonical() {
        let text = SourceText::new("let x: number = 'a';\n")
            .expect("test source fits the per-file budget");
        let diag = Diagnostic::error(
            code("2322"),
            FILE,
            range(16, 19),
            "Type 'string' is not assignable to type 'number'.",
        );
        let out = render(
            TscDiagnosticFormat::PrettyFalse,
            &[diag],
            &sources("file.ts", &text),
        );
        assert_eq!(
            out,
            "file.ts(1,17): error TS2322: Type 'string' is not assignable to type 'number'.\n"
        );
    }

    #[test]
    fn pretty_false_keeps_bamts_catalog_codes() {
        let text = SourceText::new("let x = 1;\n").expect("test source fits the per-file budget");
        let diag = Diagnostic::error(code("BAMTS-C004"), FILE, range(4, 5), "bad");
        let out = render(
            TscDiagnosticFormat::PrettyFalse,
            &[diag],
            &sources("m.ts", &text),
        );
        assert_eq!(out, "m.ts(1,5): error BAMTS-C004: bad\n");
    }

    #[test]
    fn pretty_false_does_not_fold_notes() {
        let text = SourceText::new("x").expect("test source fits the per-file budget");
        let diag = Diagnostic::error(code("2304"), FILE, range(0, 1), "Cannot find name 'x'.")
            .with_note("declared elsewhere");
        let out = render(
            TscDiagnosticFormat::PrettyFalse,
            &[diag],
            &sources("a.ts", &text),
        );
        assert_eq!(out, "a.ts(1,1): error TS2304: Cannot find name 'x'.\n");
        assert!(!out.contains("declared elsewhere"));
    }

    #[test]
    fn json_is_typed_and_stable() {
        let text = SourceText::new("x").expect("test source fits the per-file budget");
        let diag = Diagnostic::error(code("2304"), FILE, range(0, 1), "Cannot find name 'x'.");
        let out = render(TscDiagnosticFormat::Json, &[diag], &sources("a.ts", &text));
        assert_eq!(
            out,
            concat!(
                r#"[{"code":"2304","category":"error","message":"Cannot find name 'x'.","#,
                r#""file":"a.ts","span":{"start":0,"end":1},"#,
                r#""start":{"line":1,"character":1},"end":{"line":1,"character":2},"related":[]}]"#,
            )
        );
    }

    #[test]
    fn json_related_spans_and_notes() {
        let text = SourceText::new("xy").expect("test source fits the per-file budget");
        let other = SourceText::new("xy").expect("test source fits the per-file budget");
        let diag = Diagnostic::error(code("2322"), FILE, range(0, 1), "mismatch")
            .with_secondary_span(SecondarySpan::new(OTHER, range(1, 2), "declared here"))
            .with_note("see related");
        let src = vec![
            DiagnosticSource {
                id: FILE,
                name: "a.ts",
                text: &text,
            },
            DiagnosticSource {
                id: OTHER,
                name: "b.ts",
                text: &other,
            },
        ];
        let out = render(TscDiagnosticFormat::Json, &[diag], &src);
        assert!(out.contains(
            r#""related":[{"message":"declared here","file":"b.ts","span":{"start":1,"end":2}},{"message":"see related"}]"#
        ));
    }

    #[test]
    fn json_omits_file_for_unknown_source() {
        let diag = Diagnostic::error(
            code("5012"),
            FILE,
            range(0, 0),
            "Cannot read file 'missing.ts'.",
        );
        let out = render(TscDiagnosticFormat::Json, &[diag], &[]);
        assert_eq!(
            out,
            r#"[{"code":"5012","category":"error","message":"Cannot read file 'missing.ts'.","related":[]}]"#
        );
        assert!(!out.contains("\"file\""));
        assert!(!out.contains("\"span\""));
    }

    #[test]
    fn never_caps_output() {
        let text = SourceText::new("aaaaaaaaaa").expect("test source fits the per-file budget");
        let diagnostics: Vec<Diagnostic> = (0..60)
            .map(|index| {
                Diagnostic::error(
                    code("2304"),
                    FILE,
                    range(index % 10, index % 10 + 1),
                    "many",
                )
            })
            .collect();
        let pretty = render(
            TscDiagnosticFormat::PrettyFalse,
            &diagnostics,
            &sources("big.ts", &text),
        );
        let json = render(
            TscDiagnosticFormat::Json,
            &diagnostics,
            &sources("big.ts", &text),
        );
        assert_eq!(pretty.lines().count(), 60);
        assert_eq!(json.matches("\"code\":\"2304\"").count(), 60);
        assert_eq!(pretty.matches("error TS2304: many\n").count(), 60);
        let report = DiagnosticReport::new(&diagnostics);
        let (limited, notice) = crate::diagnostics::render_report(
            DiagnosticsFormat::Text,
            &report,
            &sources("big.ts", &text),
            50,
        );
        let notice = notice.expect("sixty diagnostics exceed the fifty-diagnostic limit");
        assert_eq!(notice.elided(), 10);
        assert_eq!(notice.limit(), 50);
        let notice_text = notice.render();
        assert_eq!(
            notice_text,
            "note: 10 diagnostic(s) elided after limit 50; raise with `--error-limit`"
        );
        assert_eq!(limited.lines().count(), 51);
        assert_eq!(limited.lines().last(), Some(notice_text.as_str()));
    }

    #[test]
    fn long_message_is_not_truncated() {
        let text = SourceText::new("x").expect("test source fits the per-file budget");
        let message = "this message is longer than the historical pretty-snippet window and must remain intact in pretty-false output without ellipses";
        let diag = Diagnostic::error(code("2322"), FILE, range(0, 1), message);
        let out = render(
            TscDiagnosticFormat::PrettyFalse,
            &[diag],
            &sources("a.ts", &text),
        );
        assert!(out.contains(message));
        assert!(!out.contains('…'));
    }

    #[test]
    fn utf16_columns_for_non_bmp() {
        let text = SourceText::new("😀x").expect("test source fits the per-file budget");
        let diag = Diagnostic::error(code("2322"), FILE, range(2, 3), "on x");
        let out = render(
            TscDiagnosticFormat::PrettyFalse,
            &[diag],
            &sources("e.ts", &text),
        );
        assert_eq!(out, "e.ts(1,3): error TS2322: on x\n");
    }

    #[test]
    fn empty_set_is_empty_text_and_json_array() {
        assert_eq!(render(TscDiagnosticFormat::PrettyFalse, &[], &[]), "");
        assert_eq!(render(TscDiagnosticFormat::Json, &[], &[]), "[]");
    }

    #[test]
    fn exit_status_parity() {
        let error = Diagnostic::error(code("2322"), FILE, range(0, 1), "bad");
        let warning = Diagnostic::warning(code("6133"), FILE, range(0, 1), "unused");
        assert_eq!(exit_status(&[], false), TscExitStatus::Success);
        assert_eq!(exit_status(&[warning], false), TscExitStatus::Success);
        assert_eq!(
            exit_status_from_diagnostics(std::slice::from_ref(&error), false),
            TscExitStatus::DiagnosticsPresentOutputsSkipped
        );
        assert_eq!(
            exit_status(&[error], true),
            TscExitStatus::DiagnosticsPresentOutputsGenerated
        );
    }

    #[test]
    fn from_cli_maps_text_to_pretty_false() {
        assert_eq!(
            format_from_cli(DiagnosticsFormat::Text),
            TscDiagnosticFormat::PrettyFalse
        );
        assert_eq!(
            TscDiagnosticFormat::from_cli(DiagnosticsFormat::Json),
            TscDiagnosticFormat::Json
        );
        assert_eq!(
            TscDiagnosticFormat::from_cli(DiagnosticsFormat::Pretty),
            TscDiagnosticFormat::Pretty
        );
        assert_eq!(
            format_from_cli(DiagnosticsFormat::Compact),
            TscDiagnosticFormat::PrettyFalse
        );
    }

    #[test]
    fn json_escapes_controls() {
        assert_eq!(escape_json("\"\\\n\t\u{2028}"), "\\\"\\\\\\n\\t\\u2028");
    }
}
