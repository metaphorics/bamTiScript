use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    fmt,
};

use crate::{
    lint::{LintLevel, RuleId},
    source::{SourceId, TextRange},
    syntax::NodeId,
};

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

/// How confidently a diagnostic suggestion can be applied.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Applicability {
    /// The edit is deterministic, behavior-preserving, and formatting-preserving.
    MachineApplicable,
    /// The edit has a deterministic shape but requires user-provided values.
    HasPlaceholders,
    /// The edit may change behavior or represents one of several valid choices.
    MaybeIncorrect,
    /// The remediation cannot be expressed as a localized, confidence-rated edit.
    Unspecified,
}

impl Applicability {
    /// Returns the stable name used by structured diagnostic renderers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MachineApplicable => "MachineApplicable",
            Self::HasPlaceholders => "HasPlaceholders",
            Self::MaybeIncorrect => "MaybeIncorrect",
            Self::Unspecified => "Unspecified",
        }
    }
}

impl fmt::Display for Applicability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A source edit proposed by a diagnostic.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Suggestion {
    range: TextRange,
    replacement: String,
    applicability: Applicability,
}

impl Suggestion {
    #[must_use]
    pub fn new(
        range: TextRange,
        replacement: impl Into<String>,
        applicability: Applicability,
    ) -> Self {
        Self {
            range,
            replacement: replacement.into(),
            applicability,
        }
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    #[must_use]
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    #[must_use]
    pub const fn applicability(&self) -> Applicability {
        self.applicability
    }
}

impl Ord for Suggestion {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.range.start(),
            self.range.end(),
            &self.replacement,
            self.applicability,
        )
            .cmp(&(
                other.range.start(),
                other.range.end(),
                &other.replacement,
                other.applicability,
            ))
    }
}

impl PartialOrd for Suggestion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A related source location rendered beneath a diagnostic's primary span.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SecondarySpan {
    source_id: SourceId,
    range: TextRange,
    label: String,
}

impl SecondarySpan {
    #[must_use]
    pub fn new(source_id: SourceId, range: TextRange, label: impl Into<String>) -> Self {
        Self {
            source_id,
            range,
            label: label.into(),
        }
    }

    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl Ord for SecondarySpan {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.source_id,
            self.range.start(),
            self.range.end(),
            &self.label,
        )
            .cmp(&(
                other.source_id,
                other.range.start(),
                other.range.end(),
                &other.label,
            ))
    }
}

impl PartialOrd for SecondarySpan {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PoisonedNode {
    source_id: SourceId,
    node_id: NodeId,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum DiagnosticCause {
    #[default]
    Independent,
    Root(PoisonedNode),
    Downstream(PoisonedNode),
}

/// An immutable compiler diagnostic.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Diagnostic {
    source_id: SourceId,
    range: TextRange,
    code: DiagnosticCode,
    severity: DiagnosticSeverity,
    message: Cow<'static, str>,
    rule: Option<RuleId>,
    secondary_spans: Vec<SecondarySpan>,
    note: Option<String>,
    help: Option<String>,
    suggestion: Option<Suggestion>,
    silence_instruction: Option<String>,
    cause: DiagnosticCause,
}

impl Diagnostic {
    /// Creates a diagnostic with a stable source anchor and message.
    #[must_use]
    pub fn new(
        severity: DiagnosticSeverity,
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            source_id,
            range,
            code,
            severity,
            message: message.into(),
            rule: None,
            secondary_spans: Vec::new(),
            note: None,
            help: None,
            suggestion: None,
            silence_instruction: None,
            cause: DiagnosticCause::Independent,
        }
    }

    /// Creates a recovered source error.
    #[must_use]
    pub fn error(
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(DiagnosticSeverity::Error, code, source_id, range, message)
    }

    /// Creates a non-fatal hard-warning.
    #[must_use]
    pub fn warning(
        code: DiagnosticCode,
        source_id: SourceId,
        range: TextRange,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(DiagnosticSeverity::Warning, code, source_id, range, message)
    }

    /// Creates a rule diagnostic at its resolved lint level.
    ///
    /// `allow` emits nothing, `warn` remains non-fatal, and `deny`/`forbid`
    /// produce errors that fail the build.
    #[must_use]
    pub fn lint(
        level: LintLevel,
        rule: RuleId,
        source_id: SourceId,
        range: TextRange,
        message: impl Into<Cow<'static, str>>,
    ) -> Option<Self> {
        let severity = match level {
            LintLevel::Allow => return None,
            LintLevel::Warn => DiagnosticSeverity::Warning,
            LintLevel::Deny | LintLevel::Forbid => DiagnosticSeverity::Error,
        };
        Some(
            Self::new(
                severity,
                DiagnosticCode::new(rule.code()),
                source_id,
                range,
                message,
            )
            .with_rule(rule),
        )
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
    pub fn message(&self) -> &str {
        self.message.as_ref()
    }

    /// Returns the message as a cloneable owned-or-borrowed string.
    #[must_use]
    pub fn message_cow(&self) -> Cow<'static, str> {
        self.message.clone()
    }

    /// Replaces the primary source range, preserving all other fields.
    #[must_use]
    pub fn with_range(mut self, range: TextRange) -> Self {
        self.range = range;
        self
    }

    /// Attaches the catalog identity and its copy-pasteable silence instruction.
    #[must_use]
    pub fn with_rule(mut self, rule: RuleId) -> Self {
        self.silence_instruction = Some(format!(
            "pass `-A {slug}` or set `lints.rules.{slug} = \"allow\"` in bamts.toml",
            slug = rule.slug()
        ));
        self.rule = Some(rule);
        self
    }

    /// Adds a related source span.
    #[must_use]
    pub fn with_secondary_span(mut self, span: SecondarySpan) -> Self {
        self.secondary_spans.push(span);
        self
    }

    /// Adds a technical explanation.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Adds actionable remediation guidance.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Adds a localized source edit with an explicit confidence level.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestion = Some(suggestion);
        self
    }

    /// Marks this diagnostic as the root cause that poisons one AST node.
    #[must_use]
    pub fn poisons(mut self, node_id: NodeId) -> Self {
        self.cause = DiagnosticCause::Root(PoisonedNode {
            source_id: self.source_id,
            node_id,
        });
        self
    }

    /// Marks this diagnostic as a consequence of a poisoned AST node.
    #[must_use]
    pub fn downstream_of(mut self, node_id: NodeId) -> Self {
        self.cause = DiagnosticCause::Downstream(PoisonedNode {
            source_id: self.source_id,
            node_id,
        });
        self
    }

    #[must_use]
    pub const fn rule(&self) -> Option<RuleId> {
        self.rule
    }

    #[must_use]
    pub fn secondary_spans(&self) -> &[SecondarySpan] {
        &self.secondary_spans
    }

    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    #[must_use]
    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }

    #[must_use]
    pub const fn suggestion(&self) -> Option<&Suggestion> {
        self.suggestion.as_ref()
    }

    #[must_use]
    pub fn silence_instruction(&self) -> Option<&str> {
        self.silence_instruction.as_deref()
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
            .then_with(|| self.message.cmp(&other.message))
            .then_with(|| self.rule.cmp(&other.rule))
            .then_with(|| self.secondary_spans.cmp(&other.secondary_spans))
            .then_with(|| self.note.cmp(&other.note))
            .then_with(|| self.help.cmp(&other.help))
            .then_with(|| self.suggestion.cmp(&other.suggestion))
            .then_with(|| self.silence_instruction.cmp(&other.silence_instruction))
            .then_with(|| self.cause.cmp(&other.cause))
    }
}

impl PartialOrd for Diagnostic {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The maximum number of rendered diagnostics retained for one rule.
pub const PER_RULE_DIAGNOSTIC_CAP: usize = 50;

/// Aggregate information rendered once for each rule that emitted diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleSummary {
    rule: RuleId,
    total_count: usize,
    silence_flag: String,
}

impl RuleSummary {
    #[must_use]
    pub const fn rule(&self) -> RuleId {
        self.rule
    }

    #[must_use]
    pub const fn total_count(&self) -> usize {
        self.total_count
    }

    #[must_use]
    pub fn silence_flag(&self) -> &str {
        &self.silence_flag
    }
}

/// Diagnostics prepared for presentation with noise controls applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticReport {
    diagnostics: Vec<Diagnostic>,
    summaries: Vec<RuleSummary>,
}

impl DiagnosticReport {
    /// Deduplicates, suppresses poisoned cascades, caps each rule, and summarizes totals.
    #[must_use]
    pub fn new(diagnostics: &[Diagnostic]) -> Self {
        let poisoned = diagnostics
            .iter()
            .filter_map(|diagnostic| match diagnostic.cause {
                DiagnosticCause::Root(node) => Some(node),
                DiagnosticCause::Independent | DiagnosticCause::Downstream(_) => None,
            })
            .collect::<HashSet<_>>();
        let mut ordered = diagnostics.to_vec();
        ordered.sort();

        let mut seen = HashSet::new();
        let mut totals = BTreeMap::<RuleId, usize>::new();
        let mut retained_per_rule = BTreeMap::<RuleId, usize>::new();
        let mut retained = Vec::with_capacity(ordered.len());

        for diagnostic in ordered {
            if matches!(diagnostic.cause, DiagnosticCause::Downstream(node) if poisoned.contains(&node))
            {
                continue;
            }

            let Some(rule) = diagnostic.rule else {
                retained.push(diagnostic);
                continue;
            };
            let key = (
                diagnostic.source_id,
                diagnostic.range.start(),
                diagnostic.range.end(),
                rule,
            );
            if !seen.insert(key) {
                continue;
            }

            *totals.entry(rule).or_default() += 1;
            let retained_count = retained_per_rule.entry(rule).or_default();
            if *retained_count < PER_RULE_DIAGNOSTIC_CAP {
                *retained_count += 1;
                retained.push(diagnostic);
            }
        }

        let summaries = totals
            .into_iter()
            .map(|(rule, total_count)| RuleSummary {
                rule,
                total_count,
                silence_flag: format!("-A {}", rule.slug()),
            })
            .collect();

        Self {
            diagnostics: retained,
            summaries,
        }
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn summaries(&self) -> &[RuleSummary] {
        &self.summaries
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
    use super::{
        Applicability, Diagnostic, DiagnosticCode, DiagnosticReport, DiagnosticSeverity,
        PER_RULE_DIAGNOSTIC_CAP, Recovered, SecondarySpan, Suggestion,
    };
    use crate::{
        lint::{LintLevel, RULES, RuleId},
        source::{SourceId, TextRange, Utf16Pos},
        syntax::NodeId,
    };

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered test range")
    }

    fn rule() -> RuleId {
        RULES[0].id()
    }

    fn rule_diagnostic(start: usize, end: usize) -> Diagnostic {
        let rule = rule();
        Diagnostic::warning(
            DiagnosticCode::new(rule.code()),
            SourceId::new(0),
            range(start, end),
            "rule diagnostic",
        )
        .with_rule(rule)
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

    #[test]
    fn lint_constructor_maps_all_levels_to_build_behavior() {
        assert!(
            Diagnostic::lint(
                LintLevel::Allow,
                rule(),
                SourceId::new(0),
                range(0, 1),
                "allowed",
            )
            .is_none()
        );
        let warning = Diagnostic::lint(
            LintLevel::Warn,
            rule(),
            SourceId::new(0),
            range(0, 1),
            "warning",
        )
        .expect("warn emits");
        assert_eq!(warning.severity(), DiagnosticSeverity::Warning);

        for level in [LintLevel::Deny, LintLevel::Forbid] {
            let denied = Diagnostic::lint(level, rule(), SourceId::new(0), range(0, 1), "denied")
                .expect("deny and forbid emit");
            assert_eq!(denied.severity(), DiagnosticSeverity::Error);
        }
    }

    #[test]
    fn structured_rule_metadata_is_available_to_renderers() {
        let diagnostic = rule_diagnostic(1, 3)
            .with_secondary_span(SecondarySpan::new(
                SourceId::new(1),
                range(4, 8),
                "related declaration",
            ))
            .with_note("technical explanation")
            .with_help("actionable remediation")
            .with_suggestion(Suggestion::new(
                range(1, 3),
                "replacement",
                Applicability::MachineApplicable,
            ));

        assert_eq!(diagnostic.rule(), Some(rule()));
        assert_eq!(
            diagnostic.secondary_spans()[0].source_id(),
            SourceId::new(1)
        );
        assert_eq!(diagnostic.secondary_spans()[0].range(), range(4, 8));
        assert_eq!(
            diagnostic.secondary_spans()[0].label(),
            "related declaration"
        );
        assert_eq!(diagnostic.note(), Some("technical explanation"));
        assert_eq!(diagnostic.help(), Some("actionable remediation"));
        assert_eq!(
            diagnostic.suggestion().map(Suggestion::replacement),
            Some("replacement")
        );
        assert_eq!(
            diagnostic.silence_instruction(),
            Some(
                format!(
                    "pass `-A {slug}` or set `lints.rules.{slug} = \"allow\"` in bamts.toml",
                    slug = rule().slug()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn all_suggestion_applicability_levels_have_stable_rendered_names() {
        let levels = [
            (Applicability::MachineApplicable, "MachineApplicable"),
            (Applicability::HasPlaceholders, "HasPlaceholders"),
            (Applicability::MaybeIncorrect, "MaybeIncorrect"),
            (Applicability::Unspecified, "Unspecified"),
        ];

        for (level, rendered) in levels {
            let suggestion = Suggestion::new(range(0, 1), "x", level);
            assert_eq!(suggestion.applicability(), level);
            assert_eq!(suggestion.range(), range(0, 1));
            assert_eq!(level.as_str(), rendered);
            assert_eq!(level.to_string(), rendered);
        }
    }

    #[test]
    fn report_deduplicates_by_source_span_and_rule() {
        let duplicate_with_another_message = Diagnostic::warning(
            DiagnosticCode::new(rule().code()),
            SourceId::new(0),
            range(2, 4),
            "another checker pass",
        )
        .with_rule(rule());
        let report = DiagnosticReport::new(&[
            rule_diagnostic(2, 4),
            duplicate_with_another_message,
            rule_diagnostic(2, 5),
        ]);

        assert_eq!(report.diagnostics().len(), 2);
        assert_eq!(report.summaries()[0].total_count(), 2);
    }

    #[test]
    fn report_suppresses_diagnostics_downstream_of_a_poisoned_root() {
        let poisoned_node = NodeId::new(7);
        let report = DiagnosticReport::new(&[
            rule_diagnostic(0, 1).poisons(poisoned_node),
            rule_diagnostic(1, 2).downstream_of(poisoned_node),
            rule_diagnostic(2, 3).downstream_of(poisoned_node),
            rule_diagnostic(3, 4).downstream_of(poisoned_node),
            rule_diagnostic(4, 5).downstream_of(poisoned_node),
            rule_diagnostic(5, 6).downstream_of(NodeId::new(8)),
        ]);

        assert_eq!(report.diagnostics().len(), 2);
        assert_eq!(report.summaries()[0].total_count(), 2);
        assert_eq!(report.diagnostics()[0].range(), range(0, 1));
        assert_eq!(report.diagnostics()[1].range(), range(5, 6));
    }

    #[test]
    fn report_caps_each_rule_and_summarizes_the_uncapped_total() {
        let diagnostics = (0..(PER_RULE_DIAGNOSTIC_CAP + 5))
            .map(|position| rule_diagnostic(position, position + 1))
            .collect::<Vec<_>>();
        let report = DiagnosticReport::new(&diagnostics);

        assert_eq!(report.diagnostics().len(), PER_RULE_DIAGNOSTIC_CAP);
        assert_eq!(report.summaries().len(), 1);
        let summary = &report.summaries()[0];
        assert_eq!(summary.rule(), rule());
        assert_eq!(summary.total_count(), PER_RULE_DIAGNOSTIC_CAP + 5);
        assert_eq!(summary.silence_flag(), format!("-A {}", rule().slug()));
    }
}
