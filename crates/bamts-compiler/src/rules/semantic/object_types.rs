use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{SemanticRuleContext, emit};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(
        context,
        diagnostics,
        SemanticHazard::UncheckedIndexRead,
        "BAMTS-W008",
        "index-signature read may produce undefined",
        "guard the key with `in`, an own-property check, or an explicit undefined check",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::ExplicitUndefinedOptional,
        "BAMTS-W009",
        "optional property does not explicitly admit undefined",
        "omit the property or add `undefined` to its declared value type",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::OpenObjectKeys,
        "BAMTS-W015",
        "Object.keys result is treated as an exhaustive key set",
        "keep the result as `string[]` or validate every key before indexing",
    );
    emit(
        context,
        diagnostics,
        SemanticHazard::IndexSignatureDotAccess,
        "BAMTS-W016",
        "dot access resolves only through an index signature",
        "use bracket access to make the possibly absent lookup explicit",
    );
}
