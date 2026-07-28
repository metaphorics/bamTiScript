use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{emit, SemanticRuleContext};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(context, diagnostics, SemanticHazard::DeclarationInferenceDependency, "BAMTS-W028", "exported declaration requires cross-file inference", "add an explicit public annotation using exported named types");
    emit(context, diagnostics, SemanticHazard::TypeImportedAsValue, "BAMTS-W031", "type-only symbol is imported as a value", "insert `type` in the import specifier or use `import type`");
    emit(context, diagnostics, SemanticHazard::TypeReexportedAsValue, "BAMTS-W032", "type-only symbol is re-exported as a value", "insert `type` in the export specifier or use `export type`");
}
