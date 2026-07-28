use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{emit, SemanticRuleContext};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(context, diagnostics, SemanticHazard::DivergentAccessor, "BAMTS-W011", "accessor read and write types diverge", "use one normalized property type for both accessor directions");
    emit(context, diagnostics, SemanticHazard::VirtualCallInConstructor, "BAMTS-W038", "constructor dispatches to an overridable method", "move virtual dispatch after construction or make the method non-overridable");
    emit(context, diagnostics, SemanticHazard::InitializedFieldShadowsAccessor, "BAMTS-W040", "initialized field shadows an inherited accessor", "rename the field or override the accessor deliberately");
    emit(context, diagnostics, SemanticHazard::ImplicitOverride, "BAMTS-W041", "overriding member omits the override modifier", "add the `override` modifier");
    emit(context, diagnostics, SemanticHazard::UnsafeToStringTag, "BAMTS-W080", "Symbol.toStringTag is unsafe for brand reasoning", "use a string tag and do not treat Object.prototype.toString as a trusted brand check");
    emit(context, diagnostics, SemanticHazard::UninitializedFieldShadowsAccessor, "BAMTS-W081", "uninitialized field shadows an inherited accessor", "use `declare`, initialize deliberately, or rename the field");
}
