use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{emit, SemanticRuleContext};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(context, diagnostics, SemanticHazard::NumericEnumNumber, "BAMTS-W045", "numeric enum crosses an unchecked number boundary", "make the conversion explicit at the enum boundary");
    emit(context, diagnostics, SemanticHazard::NumericEnumReverseLookup, "BAMTS-W048", "indexed access relies on a numeric enum reverse mapping", "use a forward member name or an explicit lookup table");
}
