use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{SemanticRuleContext, emit};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(
        context,
        diagnostics,
        SemanticHazard::NonExhaustiveSwitch,
        "BAMTS-W063",
        "switch does not cover every discriminated-union variant",
        "handle every reachable variant or add a deliberate default branch",
    );
}
