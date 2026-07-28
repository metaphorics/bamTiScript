use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{emit, SemanticRuleContext};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(context, diagnostics, SemanticHazard::DetachedMethod, "BAMTS-W010", "detached method call has no receiver", "call the method through its object or bind the receiver explicitly");
    emit(context, diagnostics, SemanticHazard::FewerCallbackParameters, "BAMTS-W013", "callback relies on fewer-parameter assignability", "declare the callback parameters explicitly or wrap the callback at the boundary");
    emit(context, diagnostics, SemanticHazard::ValueReturnedToVoid, "BAMTS-W014", "value-returning callback is used where its result is discarded", "use a block-bodied callback and discard the result explicitly");
    emit(context, diagnostics, SemanticHazard::ImplicitAny, "BAMTS-W018", "type inference falls back to any", "add an annotation or supply evidence from which a concrete type can be inferred");
}
