use crate::{checker::SemanticHazard, diagnostic::Diagnostic};

use super::{emit, SemanticRuleContext};

pub(super) fn analyze(context: &SemanticRuleContext<'_>, diagnostics: &mut Vec<Diagnostic>) {
    emit(context, diagnostics, SemanticHazard::InvalidNumberFormatting, "BAMTS-W071", "number formatting argument is outside the runtime range", "guard or clamp radix to 2..=36 and fraction digits to 0..=100");
    emit(context, diagnostics, SemanticHazard::NumericKeyOrder, "BAMTS-W072", "property-key order depends on integer-key reordering", "sort keys explicitly or avoid depending on insertion order");
    emit(context, diagnostics, SemanticHazard::JsonStringifyUnserializable, "BAMTS-W073", "JSON.stringify input may throw or return undefined", "validate the value or provide a replacer or primitive-returning toJSON method");
    emit(context, diagnostics, SemanticHazard::NumericDefaultSort, "BAMTS-W075", "non-string array uses the default lexicographic sort", "pass an explicit comparator");
}
