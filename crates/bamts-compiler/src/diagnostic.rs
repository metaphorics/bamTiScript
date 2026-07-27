use std::{cmp::Ordering, fmt};

use crate::source::{SourceId, TextRange};

/// A stable compiler diagnostic identifier.
///
/// Codes are static so callers cannot manufacture run-dependent identifiers that
/// would make diagnostic output unstable across equivalent compilations.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiagnosticCode(&'static str);

impl DiagnosticCode {
    /// Creates a stable diagnostic identifier.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    /// Returns the canonical identifier text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// The effect of a diagnostic on compilation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DiagnosticSeverity {
    /// The source is invalid, but recovery still supplies a product.
    Error,
    /// The source is accepted; the compiler reports a non-fatal hard-warning.
    Warning,
}

/// An immutable compiler diagnostic.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Diagnostic {
    source_id: SourceId,
    range: TextRange,
    code: DiagnosticCode,
    severity: DiagnosticSeverity,
    message: &'static str,
}

impl Diagnostic {
    /// Creates a diagnostic with a stable source anchor and message.
    #[must_use]
    pub const fn new(
        severity: DiagnosticSeverity,
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: &'static str,
    ) -> Self {
        Self {
            source_id,
            range,
            code,
            severity,
            message,
        }
    }

    /// Creates a recovered source error.
    #[must_use]
    pub const fn error(
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: &'static str,
    ) -> Self {
        Self::new(DiagnosticSeverity::Error, code, source_id, range, message)
    }

    /// Creates a non-fatal hard-warning.
    #[must_use]
    pub const fn warning(
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: &'static str,
    ) -> Self {
        Self::new(DiagnosticSeverity::Warning, code, source_id, range, message)
    }

    /// Returns the source containing this diagnostic.
    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    /// Returns the immutable source range that triggered this diagnostic.
    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    /// Returns the stable diagnostic identifier.
    #[must_use]
    pub const fn code(&self) -> DiagnosticCode {
        self.code
    }

    /// Returns whether this diagnostic is an error or a warning.
    #[must_use]
    pub const fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    /// Returns the exact compiler message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Returns whether this diagnostic is non-fatal.
    #[must_use]
    pub const fn is_warning(&self) -> bool {
        matches!(self.severity, DiagnosticSeverity::Warning)
    }
}

impl Ord for Diagnostic {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.source_id,
            self.range.start(),
            self.range.end(),
            self.code,
        )
            .cmp(&(
                other.source_id,
                other.range.start(),
                other.range.end(),
                other.code,
            ))
            .then_with(|| self.severity.cmp(&other.severity))
            .then_with(|| self.message.cmp(other.message))
    }
}

impl PartialOrd for Diagnostic {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A compiler product retained even when recovery emits diagnostics.
///
/// `Recovered` deliberately never uses `Result`: syntax and type problems are
/// compiler data, while callers always retain an inspectable product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recovered<T> {
    product: T,
    diagnostics: Vec<Diagnostic>,
}

impl<T> Recovered<T> {
    /// Retains `product` and canonically orders its diagnostics.
    #[must_use]
    pub fn new(product: T, mut diagnostics: Vec<Diagnostic>) -> Self {
        diagnostics.sort();
        Self {
            product,
            diagnostics,
        }
    }

    /// Wraps a product with no diagnostics.
    #[must_use]
    pub fn clean(product: T) -> Self {
        Self::new(product, Vec::new())
    }

    /// Returns the recovered product.
    #[must_use]
    pub const fn product(&self) -> &T {
        &self.product
    }

    /// Consumes the wrapper while retaining the recovered product.
    #[must_use]
    pub fn into_product(self) -> T {
        self.product
    }

    /// Returns diagnostics in canonical order.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Consumes the wrapper into its product and canonically ordered diagnostics.
    #[must_use]
    pub fn into_parts(self) -> (T, Vec<Diagnostic>) {
        (self.product, self.diagnostics)
    }

    /// Transforms the retained product without discarding recovery diagnostics.
    #[must_use]
    pub fn map<U>(self, transform: impl FnOnce(T) -> U) -> Recovered<U> {
        Recovered {
            product: transform(self.product),
            diagnostics: self.diagnostics,
        }
    }

    /// Returns a new wrapper with one additional diagnostic in canonical order.
    #[must_use]
    pub fn with_diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        let insertion = self
            .diagnostics
            .partition_point(|existing| existing <= &diagnostic);
        self.diagnostics.insert(insertion, diagnostic);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, DiagnosticCode, DiagnosticSeverity, Recovered};
    use crate::source::{SourceId, TextRange, Utf16Pos};

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered test range")
    }

    #[test]
    fn recovered_keeps_the_product_alongside_errors() {
        let recovered = Recovered::new(
            String::from("usable syntax tree"),
            vec![Diagnostic::error(
                DiagnosticCode::new("BAMTS-E001"),
                SourceId::new(0),
                range(0, 1),
                "expected expression",
            )],
        );

        assert_eq!(recovered.product(), "usable syntax tree");
        assert_eq!(recovered.diagnostics().len(), 1);
        assert_eq!(
            recovered.diagnostics()[0].severity(),
            DiagnosticSeverity::Error
        );
    }

    #[test]
    fn diagnostics_sort_by_the_contract_key() {
        let recovered = Recovered::new(
            (),
            vec![
                Diagnostic::warning(
                    DiagnosticCode::new("BAMTS-W007"),
                    SourceId::new(1),
                    range(0, 1),
                    "later source",
                ),
                Diagnostic::warning(
                    DiagnosticCode::new("BAMTS-W002"),
                    SourceId::new(0),
                    range(3, 4),
                    "later position",
                ),
                Diagnostic::warning(
                    DiagnosticCode::new("BAMTS-W003"),
                    SourceId::new(0),
                    range(1, 2),
                    "higher code",
                ),
                Diagnostic::warning(
                    DiagnosticCode::new("BAMTS-W001"),
                    SourceId::new(0),
                    range(1, 2),
                    "lower code",
                ),
            ],
        );

        let codes = recovered
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            ["BAMTS-W001", "BAMTS-W003", "BAMTS-W002", "BAMTS-W007"]
        );
    }

    #[test]
    fn warnings_remain_warnings() {
        let warning = Diagnostic::warning(
            DiagnosticCode::new("BAMTS-W001"),
            SourceId::new(0),
            range(0, 1),
            "method parameter bivariance",
        );

        assert!(warning.is_warning());
        assert_eq!(warning.severity(), DiagnosticSeverity::Warning);
    }
}
