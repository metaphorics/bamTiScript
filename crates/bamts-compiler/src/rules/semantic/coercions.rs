use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{SemanticRuleContext, emit};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(
        context,
        diagnostics,
        SemanticHazard::LooseEqualityCoercion,
        "BAMTS-W076",
        "loose equality depends on implicit coercion",
        "use strict equality or convert both operands explicitly",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::ObjectToPrimitive,
        "BAMTS-W077",
        "object is implicitly coerced to a primitive",
        "call String, Number, or an explicit conversion method",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::SymbolInterpolation,
        "BAMTS-W078",
        "symbol interpolation throws during template conversion",
        "wrap the symbol with String or interpolate its description",
    );
}
