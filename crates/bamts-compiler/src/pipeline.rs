//! Compiler-owned frontend orchestration.
//!
//! [`compile_frontend`] drives the fixed scan -> parse -> check -> optional emit
//! pipeline over one source and returns a single recovered [`FrontendOutput`].
//! The pipeline never stops on user diagnostics: every stage always yields a
//! product, and their diagnostics are unioned into one canonically ordered,
//! duplicate-free vector without losing any distinct severity, range, or code.
//!
//! This module owns no filesystem, CLI, lowerer, runtime, or backend. It only
//! composes the existing scanner, parser, checker, and emitter surfaces.

use std::sync::Arc;

use crate::checker::{self, SemanticModel};
use crate::diagnostic::Diagnostic;
use crate::emitter::{self, EmitOptions, EmitOutput};
use crate::lint::{LintProfile, LintTable};
use crate::parser;
use crate::scanner;
use crate::source::{ScriptKind, SourceId, SourceText};
use crate::syntax::SourceFile;

/// The frontend product a caller wants produced for one source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrontendMode {
    /// Scan, parse, and type-check only; no emit is produced.
    Check,
    /// Additionally emit runtime JavaScript with type-only syntax erased.
    JavaScript,
    /// Additionally emit a TypeScript declaration file.
    Declaration,
}

impl FrontendMode {
    /// Returns the emit options this mode requests, or `None` for check-only.
    #[must_use]
    const fn emit_options(self) -> Option<EmitOptions> {
        match self {
            Self::Check => None,
            Self::JavaScript => Some(EmitOptions::javascript()),
            Self::Declaration => Some(EmitOptions::declaration()),
        }
    }
}

/// One immutable frontend compilation request.
#[derive(Clone, Debug)]
pub struct FrontendRequest {
    /// The compilation-assigned identity of the source.
    pub source_id: SourceId,
    /// The syntax accepted for the source.
    pub script_kind: ScriptKind,
    /// The shared, immutable source text.
    pub source: Arc<SourceText>,
    /// The frontend product to produce.
    pub mode: FrontendMode,
}

/// The immutable product of a full frontend compilation.
///
/// The recovered [`SourceFile`] and [`SemanticModel`] are always present, the
/// [`EmitOutput`] is present exactly when the request asked for one, and
/// [`FrontendOutput::diagnostics`] is the single canonically ordered union of
/// every stage's diagnostics.
pub struct FrontendOutput {
    mode: FrontendMode,
    source_file: SourceFile,
    semantic_model: SemanticModel,
    emit: Option<EmitOutput>,
    diagnostics: Vec<Diagnostic>,
}

impl FrontendOutput {
    /// Returns the mode this output was produced for.
    #[must_use]
    pub const fn mode(&self) -> FrontendMode {
        self.mode
    }

    /// Returns the recovered parser product, always present even on syntax errors.
    #[must_use]
    pub const fn source_file(&self) -> &SourceFile {
        &self.source_file
    }

    /// Returns the recovered semantic model, always present even on type errors.
    #[must_use]
    pub const fn semantic_model(&self) -> &SemanticModel {
        &self.semantic_model
    }

    /// Returns the emit product, present exactly when the request asked to emit.
    #[must_use]
    pub const fn emit(&self) -> Option<&EmitOutput> {
        self.emit.as_ref()
    }

    /// Returns every stage's diagnostics in canonical order with no duplicates.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Returns whether any diagnostic is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| !diagnostic.is_warning())
    }

    /// Consumes the output into its parts.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        SourceFile,
        SemanticModel,
        Option<EmitOutput>,
        Vec<Diagnostic>,
    ) {
        (
            self.source_file,
            self.semantic_model,
            self.emit,
            self.diagnostics,
        )
    }
}

/// Runs the fixed frontend pipeline with settled default lint levels.
#[must_use]
pub fn compile_frontend(request: FrontendRequest) -> FrontendOutput {
    compile_frontend_with_lints(request, &LintTable::new(LintProfile::Default))
}

/// Runs the fixed scan -> parse -> check -> optional emit frontend pipeline
/// using the caller's resolved lint table.
///
/// Every stage runs regardless of the diagnostics its predecessor produced, so
/// the returned [`FrontendOutput`] always carries a recovered `SourceFile` and
/// `SemanticModel`, and (for emitting modes) an [`EmitOutput`] even when earlier
/// stages reported errors. All stage diagnostics are merged, canonically
/// ordered, and de-duplicated into one vector.
#[must_use]
pub fn compile_frontend_with_lints(request: FrontendRequest, levels: &LintTable) -> FrontendOutput {
    let FrontendRequest {
        source_id,
        script_kind,
        source,
        mode,
    } = request;

    let scanned = scanner::scan(source_id, script_kind, source);
    let parsed = parser::parse(scanned);
    let checked = checker::check_with_lints(&parsed, levels);

    // Emit runs against the recovered tree; it never gates on prior diagnostics.
    let emit = mode
        .emit_options()
        .map(|options| emitter::emit(parsed.product(), options));

    let (source_file, parse_diagnostics) = parsed.into_parts();
    let (semantic_model, check_diagnostics) = checked.into_parts();

    let mut diagnostics = parse_diagnostics;
    diagnostics.extend(check_diagnostics);
    if let Some(output) = &emit {
        diagnostics.extend(output.diagnostics.iter().cloned());
    }
    let diagnostics = canonicalize(diagnostics);

    FrontendOutput {
        mode,
        source_file,
        semantic_model,
        emit,
        diagnostics,
    }
}

/// Orders diagnostics by the canonical [`Diagnostic`] key and removes exact
/// duplicates.
///
/// The canonical order is a total order over `(source, range, code, severity,
/// message)`, so sorting groups every identical diagnostic and `dedup` collapses
/// only exact duplicates: two diagnostics differing in any of severity, range,
/// or code are never merged.
fn canonicalize(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort();
    diagnostics.dedup();
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::{FrontendMode, FrontendRequest, canonicalize, compile_frontend};
    use crate::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
    use crate::source::{ScriptKind, SourceId, SourceText, TextRange, Utf16Pos};
    use std::sync::Arc;

    fn request(source: &str, mode: FrontendMode) -> FrontendRequest {
        FrontendRequest {
            source_id: SourceId::new(0),
            script_kind: ScriptKind::TypeScript,
            source: Arc::new(SourceText::new(source)),
            mode,
        }
    }

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered range")
    }

    fn has_code(diagnostics: &[Diagnostic], code: &str) -> bool {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code().as_str() == code)
    }

    fn is_sorted_unique(diagnostics: &[Diagnostic]) -> bool {
        diagnostics.windows(2).all(|pair| pair[0] < pair[1])
    }

    #[test]
    fn merges_syntax_type_and_warning_diagnostics_into_one_ordered_vector() {
        // Warning (unchecked catch property access), a type error (string not
        // assignable to number), and a trailing syntax error (missing initializer)
        // all coexist in one source.
        let source =
            "try {} catch (e) { e.message }\nconst n: number = \"oops\";\nconst bad: number =";
        let output = compile_frontend(request(source, FrontendMode::Check));
        let diagnostics = output.diagnostics();

        // The three stages each contributed at least one diagnostic.
        assert!(has_code(diagnostics, "BAMTS-W005"), "warning stage missing");
        assert!(has_code(diagnostics, "BAMTS-C004"), "type stage missing");
        assert!(
            diagnostics.iter().any(
                |diagnostic| diagnostic.severity() == DiagnosticSeverity::Error
                    && diagnostic.code().as_str() != "BAMTS-C004"
            ),
            "syntax stage missing",
        );

        // Both severities survive the merge.
        assert!(diagnostics.iter().any(Diagnostic::is_warning));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| !diagnostic.is_warning())
        );

        // The single vector is canonically ordered and free of exact duplicates.
        assert!(is_sorted_unique(diagnostics), "diagnostics not canonical");
    }

    #[test]
    fn emits_despite_earlier_errors() {
        // A type error must not suppress the emit product.
        let source = "const n: number = \"oops\";";
        let output = compile_frontend(request(source, FrontendMode::JavaScript));

        assert!(output.has_errors(), "expected a type error");
        let emit = output.emit().expect("javascript mode must emit");
        assert!(
            emit.code.contains("oops"),
            "emit should still print the recovered program",
        );
    }

    #[test]
    fn check_mode_produces_no_emit_while_js_and_declaration_do() {
        let source = "let value: number = 1;";

        let check = compile_frontend(request(source, FrontendMode::Check));
        assert!(check.emit().is_none(), "check mode must not emit");

        let js = compile_frontend(request(source, FrontendMode::JavaScript));
        let js_emit = js.emit().expect("javascript mode emits");

        let declaration = compile_frontend(request(source, FrontendMode::Declaration));
        let declaration_emit = declaration.emit().expect("declaration mode emits");

        // JavaScript erases the type annotation; the declaration surface keeps it.
        assert!(!js_emit.code.contains("number"), "js must erase the type");
        assert!(
            declaration_emit.code.contains("number"),
            "declaration must retain the type",
        );
        assert_ne!(js_emit.code, declaration_emit.code);
    }

    #[test]
    fn canonicalize_collapses_exact_duplicates_and_preserves_distinct_ones() {
        let source_id = SourceId::new(0);
        let code = DiagnosticCode::new("BAMTS-C001");
        let base = Diagnostic::error(code, source_id, range(0, 1), "duplicate");
        let duplicate = base.clone();
        // Distinct in severity only.
        let as_warning = Diagnostic::warning(code, source_id, range(0, 1), "duplicate");
        // Distinct in range only.
        let elsewhere = Diagnostic::error(code, source_id, range(2, 3), "duplicate");
        // Distinct in code only.
        let other_code = Diagnostic::error(
            DiagnosticCode::new("BAMTS-C002"),
            source_id,
            range(0, 1),
            "duplicate",
        );

        let merged = canonicalize(vec![
            base.clone(),
            duplicate,
            as_warning.clone(),
            elsewhere.clone(),
            other_code.clone(),
            base.clone(),
        ]);

        // The two exact copies collapse to one; every distinct diagnostic remains.
        assert_eq!(merged.len(), 4);
        assert_eq!(merged.iter().filter(|d| **d == base).count(), 1);
        assert!(merged.contains(&as_warning));
        assert!(merged.contains(&elsewhere));
        assert!(merged.contains(&other_code));
        assert!(is_sorted_unique(&merged));
    }

    #[test]
    fn frontend_output_never_contains_duplicate_diagnostics() {
        let source =
            "try {} catch (e) { e.message }\nconst n: number = \"oops\";\nconst bad: number =";
        let output = compile_frontend(request(source, FrontendMode::JavaScript));
        assert!(is_sorted_unique(output.diagnostics()));
    }
}
