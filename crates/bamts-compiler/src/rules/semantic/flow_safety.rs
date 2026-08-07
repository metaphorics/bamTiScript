use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{SemanticRuleContext, emit};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(
        context,
        diagnostics,
        SemanticHazard::ReadonlyAliasMutation,
        "BAMTS-W012",
        "mutable alias writes through an observable readonly view",
        "copy before mutation or keep one consistently readonly view",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::UncheckedAssertion,
        "BAMTS-W019",
        "type assertion is not justified by control-flow analysis",
        "narrow with a runtime check or validated decoder before asserting",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::UncheckedJsonParse,
        "BAMTS-W074",
        "JSON.parse result reaches a trusted type without validation",
        "receive the result as unknown and validate or decode it",
    );
}
