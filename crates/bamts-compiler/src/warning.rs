use crate::{
    diagnostic::{Diagnostic, Recovered},
    lint::{LintLevel, LintProfile, LintTable, RULES},
    rules,
    source::{ScriptKind, SourceId, SourceText},
    syntax::SourceFile,
};

const MAX_RECOGNIZER_TOKENS: usize = 4_096;
const MAX_PATTERN_TOKENS: usize = 128;

/// Analyzes the recovered syntax product using the default lint profile.
///
/// Existing recovery diagnostics are intentionally neither read nor mutated.
#[must_use]
pub fn analyze_hard_warnings(source_file: &Recovered<SourceFile>) -> Vec<Diagnostic> {
    analyze_warnings(source_file, &LintTable::new(LintProfile::Default))
}

/// Analyzes the recovered syntax product using a resolved lint table.
#[must_use]
pub fn analyze_warnings(
    source_file: &Recovered<SourceFile>,
    levels: &LintTable,
) -> Vec<Diagnostic> {
    let source_file = source_file.product();
    let mut diagnostics = if matches!(
        source_file.script_kind(),
        ScriptKind::TypeScript | ScriptKind::TypeScriptReact
    ) {
        analyze_source_text(source_file.source_id(), source_file.source_text())
    } else {
        Vec::new()
    };
    diagnostics.extend(rules::analyze(source_file, levels));
    diagnostics.sort();
    diagnostics
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LexemeKind {
    Identifier,
    Number,
    String,
    Punctuation,
}

#[derive(Clone, Copy, Debug)]
struct Lexeme<'source> {
    text: &'source str,
    start_byte: usize,
    end_byte: usize,
    kind: LexemeKind,
}

impl Lexeme<'_> {
    fn is(&self, expected: &str) -> bool {
        self.text == expected
    }

    fn is_identifier(&self) -> bool {
        matches!(self.kind, LexemeKind::Identifier)
    }
}

#[derive(Clone, Copy)]
struct ArrayBinding<'source> {
    name: &'source str,
    element: &'source str,
}

struct ObjectBinding<'source> {
    name: &'source str,
    properties: Vec<&'source str>,
    declared_at: usize,
}

struct RequiredPropertyBinding<'source> {
    object: &'source str,
    properties: Vec<&'source str>,
    declared_at: usize,
}

#[derive(Clone, Copy)]
struct TupleBinding<'source> {
    name: &'source str,
    declared_at: usize,
}

fn analyze_source_text(source_id: SourceId, source: &SourceText) -> Vec<Diagnostic> {
    let lexemes = lex(source.as_str());
    let mut diagnostics = Vec::new();

    recognize_method_parameter_bivariance(&lexemes, source_id, source, &mut diagnostics);
    recognize_mutable_array_covariance(&lexemes, source_id, source, &mut diagnostics);
    recognize_non_fresh_excess_property_bypass(&lexemes, source_id, source, &mut diagnostics);
    recognize_delete_required_property(&lexemes, source_id, source, &mut diagnostics);
    recognize_unchecked_catch_property_access(&lexemes, source_id, source, &mut diagnostics);
    recognize_generic_any_downcast(&lexemes, source_id, source, &mut diagnostics);
    recognize_dynamic_tuple_index(&lexemes, source_id, source, &mut diagnostics);

    diagnostics.sort();
    diagnostics
}

fn lex(source: &str) -> Vec<Lexeme<'_>> {
    let bytes = source.as_bytes();
    let mut lexemes = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() && lexemes.len() < MAX_RECOGNIZER_TOKENS {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }

        if byte == b'/' && bytes.get(cursor + 1) == Some(&b'/') {
            cursor = source[cursor..]
                .find('\n')
                .map_or(bytes.len(), |offset| cursor + offset + 1);
            continue;
        }
        if byte == b'/' && bytes.get(cursor + 1) == Some(&b'*') {
            cursor = source[cursor + 2..]
                .find("*/")
                .map_or(bytes.len(), |offset| cursor + offset + 4);
            continue;
        }
        if byte == b'/' && starts_regex_literal(&lexemes) {
            let start = cursor;
            cursor = consume_regex_literal(bytes, cursor);
            lexemes.push(Lexeme {
                text: &source[start..cursor],
                start_byte: start,
                end_byte: cursor,
                kind: LexemeKind::String,
            });
            continue;
        }

        if matches!(byte, b'\'' | b'\"' | b'`') {
            let start = cursor;
            cursor = consume_string(bytes, cursor, byte);
            lexemes.push(Lexeme {
                text: &source[start..cursor],
                start_byte: start,
                end_byte: cursor,
                kind: LexemeKind::String,
            });
            continue;
        }

        if is_identifier_start(byte) {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                cursor += 1;
            }
            lexemes.push(Lexeme {
                text: &source[start..cursor],
                start_byte: start,
                end_byte: cursor,
                kind: LexemeKind::Identifier,
            });
            continue;
        }

        if byte.is_ascii_digit() {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            lexemes.push(Lexeme {
                text: &source[start..cursor],
                start_byte: start,
                end_byte: cursor,
                kind: LexemeKind::Number,
            });
            continue;
        }

        let start = cursor;
        let Some(character) = source[cursor..].chars().next() else {
            break;
        };
        cursor += character.len_utf8();
        lexemes.push(Lexeme {
            text: &source[start..cursor],
            start_byte: start,
            end_byte: cursor,
            kind: LexemeKind::Punctuation,
        });
    }

    lexemes
}

fn consume_string(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
            continue;
        }
        cursor += 1;
        if bytes[cursor - 1] == quote {
            break;
        }
    }
    cursor
}
fn starts_regex_literal(lexemes: &[Lexeme<'_>]) -> bool {
    lexemes.last().is_none_or(|previous| {
        matches!(
            previous.text,
            "=" | "(" | "{" | "[" | "," | ":" | ";" | "!" | "?" | "return" | "throw"
        )
    })
}

fn consume_regex_literal(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    let mut in_character_class = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            b'\n' | b'\r' => break,
            b'[' => {
                in_character_class = true;
                cursor += 1;
            }
            b']' => {
                in_character_class = false;
                cursor += 1;
            }
            b'/' if !in_character_class => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                    cursor += 1;
                }
                break;
            }
            _ => cursor += 1,
        }
    }
    cursor
}

const fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

const fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn recognize_method_parameter_bivariance(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: a typed method declaration directly in an interface body. Function-valued
    // properties, free functions, classes, and untyped methods deliberately do not match.
    for index in 0..lexemes.len() {
        if !lexemes[index].is("interface")
            || !lexemes.get(index + 1).is_some_and(Lexeme::is_identifier)
            || !lexemes.get(index + 2).is_some_and(|lexeme| lexeme.is("{"))
        {
            continue;
        }

        let Some(body_end) = matching_delimiter(lexemes, index + 2, "{", "}") else {
            continue;
        };
        let mut member = index + 3;
        while member < body_end {
            let Some(method_name) = lexemes.get(member) else {
                break;
            };
            if !method_name.is_identifier()
                || !lexemes.get(member + 1).is_some_and(|lexeme| lexeme.is("("))
            {
                member += 1;
                continue;
            }
            let Some(parameters_end) = matching_delimiter(lexemes, member + 1, "(", ")") else {
                break;
            };
            let has_typed_parameter = lexemes[member + 2..parameters_end]
                .windows(2)
                .any(|pair| pair[0].is_identifier() && pair[1].is(":"));
            if has_typed_parameter
                && lexemes
                    .get(parameters_end + 1)
                    .is_some_and(|lexeme| lexeme.is(":"))
            {
                emit(diagnostics, source_id, source, 0, method_name);
            }
            member = parameters_end + 1;
        }
    }
}

fn recognize_mutable_array_covariance(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: two explicit single-name mutable array declarations, where the second
    // initializes from the first and their element type names differ. Assertions, readonly
    // arrays, expressions, and matching element names deliberately do not match.
    let mut bindings = Vec::new();
    for index in 0..lexemes.len() {
        let Some((name, element, after_type)) = typed_array_declaration(lexemes, index) else {
            continue;
        };

        if lexemes.get(after_type).is_some_and(|lexeme| lexeme.is("="))
            && lexemes
                .get(after_type + 1)
                .is_some_and(Lexeme::is_identifier)
            && declaration_ends_after(lexemes, after_type + 2)
            && bindings.iter().rev().any(|binding: &ArrayBinding<'_>| {
                binding.name == lexemes[after_type + 1].text && binding.element != element
            })
        {
            emit(diagnostics, source_id, source, 1, &lexemes[after_type + 1]);
        }

        bindings.push(ArrayBinding { name, element });
    }
}

fn recognize_non_fresh_excess_property_bypass(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: a named object literal with direct identifier keys subsequently assigned to a
    // typed object literal with a strict subset of those keys. Fresh object literals and calls
    // are deliberately excluded because this recognizer is only about the non-fresh escape.
    let bindings = object_literal_bindings(lexemes);
    for index in 0..lexemes.len() {
        let Some((after_type, expected)) = typed_object_declaration(lexemes, index) else {
            continue;
        };
        if expected.is_empty()
            || !lexemes.get(after_type).is_some_and(|lexeme| lexeme.is("="))
            || !lexemes
                .get(after_type + 1)
                .is_some_and(Lexeme::is_identifier)
            || !declaration_ends_after(lexemes, after_type + 2)
        {
            continue;
        }

        let value = &lexemes[after_type + 1];
        let Some(binding) = bindings
            .iter()
            .rev()
            .find(|binding| binding.declared_at < index && binding.name == value.text)
        else {
            continue;
        };
        if binding
            .properties
            .iter()
            .any(|property| !expected.contains(property))
        {
            emit(diagnostics, source_id, source, 2, value);
        }
    }
}

fn recognize_delete_required_property(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: `delete object.property` where `object` was declared with a direct object
    // literal type that names `property` without `?`. Optional properties and unknown object
    // shapes are deliberately excluded.
    let bindings = required_property_bindings(lexemes);
    for index in 0..lexemes.len().saturating_sub(3) {
        if !lexemes[index].is("delete")
            || !lexemes[index + 1].is_identifier()
            || !lexemes[index + 2].is(".")
            || !lexemes[index + 3].is_identifier()
        {
            continue;
        }
        let object = lexemes[index + 1].text;
        let property = lexemes[index + 3].text;
        if bindings.iter().any(|binding| {
            binding.declared_at < index
                && binding.object == object
                && binding.properties.contains(&property)
        }) {
            emit(diagnostics, source_id, source, 3, &lexemes[index + 3]);
        }
    }
}

fn recognize_unchecked_catch_property_access(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: the first statement in a catch body is a direct `binding.property` access.
    // Guarded branches, indirect access, and property uses after another statement are outside
    // this intentionally conservative temporary recognizer.
    for index in 0..lexemes.len().saturating_sub(7) {
        if !lexemes[index].is("catch")
            || !lexemes[index + 1].is("(")
            || !lexemes[index + 2].is_identifier()
            || !lexemes[index + 3].is(")")
            || !lexemes[index + 4].is("{")
            || !lexemes[index + 5].is_identifier()
            || lexemes[index + 2].text != lexemes[index + 5].text
            || !lexemes[index + 6].is(".")
            || !lexemes[index + 7].is_identifier()
        {
            continue;
        }
        emit(diagnostics, source_id, source, 4, &lexemes[index + 7]);
    }
}

fn recognize_generic_any_downcast(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: a `function` with one generic name, a direct `any` parameter, the same generic
    // return annotation, and a direct `return parameter as Generic`. Unknown, constrained, and
    // non-return casts do not match.
    for index in 0..lexemes.len() {
        if !lexemes[index].is("function")
            || !lexemes.get(index + 1).is_some_and(Lexeme::is_identifier)
            || !lexemes.get(index + 2).is_some_and(|lexeme| lexeme.is("<"))
            || !lexemes.get(index + 3).is_some_and(Lexeme::is_identifier)
            || !lexemes.get(index + 4).is_some_and(|lexeme| lexeme.is(">"))
            || !lexemes.get(index + 5).is_some_and(|lexeme| lexeme.is("("))
        {
            continue;
        }

        let generic = lexemes[index + 3].text;
        let Some(parameters_end) = matching_delimiter(lexemes, index + 5, "(", ")") else {
            continue;
        };
        if !lexemes
            .get(parameters_end + 1)
            .is_some_and(|lexeme| lexeme.is(":"))
            || !lexemes
                .get(parameters_end + 2)
                .is_some_and(|lexeme| lexeme.text == generic)
            || !lexemes
                .get(parameters_end + 3)
                .is_some_and(|lexeme| lexeme.is("{"))
        {
            continue;
        }

        let Some((parameter, any_token)) = direct_any_parameter(lexemes, index + 6, parameters_end)
        else {
            continue;
        };
        let body_start = parameters_end + 3;
        let Some(body_end) = matching_delimiter(lexemes, body_start, "{", "}") else {
            continue;
        };
        if has_direct_generic_any_return(lexemes, body_start + 1, body_end, parameter, generic) {
            emit(diagnostics, source_id, source, 5, any_token);
        }
    }
}

fn recognize_dynamic_tuple_index(
    lexemes: &[Lexeme<'_>],
    source_id: SourceId,
    source: &SourceText,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Boundary: a variable has an explicit bracketed tuple type containing a comma, then that
    // same variable is indexed by one identifier. Literal indexes and ordinary arrays do not
    // match, so the warning is limited to dynamic tuple indexing.
    let bindings = tuple_bindings(lexemes);
    for index in 0..lexemes.len().saturating_sub(3) {
        if !lexemes[index].is_identifier()
            || !lexemes[index + 1].is("[")
            || !lexemes[index + 2].is_identifier()
            || !lexemes[index + 3].is("]")
        {
            continue;
        }
        if bindings
            .iter()
            .any(|binding| binding.declared_at < index && binding.name == lexemes[index].text)
        {
            emit(diagnostics, source_id, source, 6, &lexemes[index + 2]);
        }
    }
}

fn typed_array_declaration<'source>(
    lexemes: &'source [Lexeme<'source>],
    index: usize,
) -> Option<(&'source str, &'source str, usize)> {
    if !is_declaration(lexemes.get(index))
        || !lexemes.get(index + 1)?.is_identifier()
        || !lexemes.get(index + 2)?.is(":")
        || !lexemes.get(index + 3)?.is_identifier()
        || !lexemes.get(index + 4)?.is("[")
        || !lexemes.get(index + 5)?.is("]")
    {
        return None;
    }
    Some((lexemes[index + 1].text, lexemes[index + 3].text, index + 6))
}

fn object_literal_bindings<'source>(
    lexemes: &'source [Lexeme<'source>],
) -> Vec<ObjectBinding<'source>> {
    let mut bindings = Vec::new();
    for index in 0..lexemes.len() {
        if !is_declaration(lexemes.get(index))
            || !lexemes.get(index + 1).is_some_and(Lexeme::is_identifier)
            || !lexemes.get(index + 2).is_some_and(|lexeme| lexeme.is("="))
            || !lexemes.get(index + 3).is_some_and(|lexeme| lexeme.is("{"))
        {
            continue;
        }
        let Some(object_end) = matching_delimiter(lexemes, index + 3, "{", "}") else {
            continue;
        };
        let properties = direct_object_properties(lexemes, index + 4, object_end);
        if !properties.is_empty() {
            bindings.push(ObjectBinding {
                name: lexemes[index + 1].text,
                properties,
                declared_at: index,
            });
        }
    }
    bindings
}

fn typed_object_declaration<'source>(
    lexemes: &'source [Lexeme<'source>],
    index: usize,
) -> Option<(usize, Vec<&'source str>)> {
    if !is_declaration(lexemes.get(index))
        || !lexemes.get(index + 1).is_some_and(Lexeme::is_identifier)
        || !lexemes.get(index + 2).is_some_and(|lexeme| lexeme.is(":"))
        || !lexemes.get(index + 3).is_some_and(|lexeme| lexeme.is("{"))
    {
        return None;
    }
    let type_end = matching_delimiter(lexemes, index + 3, "{", "}")?;
    Some((
        type_end + 1,
        direct_object_properties(lexemes, index + 4, type_end),
    ))
}

fn required_property_bindings<'source>(
    lexemes: &'source [Lexeme<'source>],
) -> Vec<RequiredPropertyBinding<'source>> {
    let mut bindings = Vec::new();
    for index in 0..lexemes.len() {
        let Some((after_type, _)) = typed_object_declaration(lexemes, index) else {
            continue;
        };
        let required = direct_required_object_properties(lexemes, index + 4, after_type - 1);
        if !required.is_empty() {
            bindings.push(RequiredPropertyBinding {
                object: lexemes[index + 1].text,
                properties: required,
                declared_at: index,
            });
        }
    }
    bindings
}

fn tuple_bindings<'source>(lexemes: &'source [Lexeme<'source>]) -> Vec<TupleBinding<'source>> {
    let mut bindings = Vec::new();
    for index in 0..lexemes.len() {
        if !is_declaration(lexemes.get(index))
            || !lexemes.get(index + 1).is_some_and(Lexeme::is_identifier)
            || !lexemes.get(index + 2).is_some_and(|lexeme| lexeme.is(":"))
            || !lexemes.get(index + 3).is_some_and(|lexeme| lexeme.is("["))
        {
            continue;
        }
        let Some(tuple_end) = matching_delimiter(lexemes, index + 3, "[", "]") else {
            continue;
        };
        if lexemes[index + 4..tuple_end]
            .iter()
            .any(|lexeme| lexeme.is(","))
        {
            bindings.push(TupleBinding {
                name: lexemes[index + 1].text,
                declared_at: index,
            });
        }
    }
    bindings
}

fn direct_object_properties<'source>(
    lexemes: &'source [Lexeme<'source>],
    start: usize,
    end: usize,
) -> Vec<&'source str> {
    let mut properties = Vec::new();
    let mut depth: usize = 0;
    for index in start..end {
        match lexemes[index].text {
            "{" | "[" | "(" => depth += 1,
            "}" | "]" | ")" => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0
            && lexemes[index].is_identifier()
            && lexemes
                .get(index + 1)
                .is_some_and(|next| next.is(":") || next.is("?"))
        {
            properties.push(lexemes[index].text);
        }
    }
    properties
}

fn direct_required_object_properties<'source>(
    lexemes: &'source [Lexeme<'source>],
    start: usize,
    end: usize,
) -> Vec<&'source str> {
    let mut properties = Vec::new();
    let mut depth: usize = 0;
    for index in start..end {
        match lexemes[index].text {
            "{" | "[" | "(" => depth += 1,
            "}" | "]" | ")" => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0
            && lexemes[index].is_identifier()
            && lexemes.get(index + 1).is_some_and(|next| next.is(":"))
        {
            properties.push(lexemes[index].text);
        }
    }
    properties
}

fn direct_any_parameter<'source>(
    lexemes: &'source [Lexeme<'source>],
    start: usize,
    end: usize,
) -> Option<(&'source str, &'source Lexeme<'source>)> {
    let parameters = &lexemes[start..end];
    for pair in parameters.windows(3) {
        if pair[0].is_identifier() && pair[1].is(":") && pair[2].is("any") {
            return Some((pair[0].text, &pair[2]));
        }
    }
    None
}

fn has_direct_generic_any_return(
    lexemes: &[Lexeme<'_>],
    start: usize,
    end: usize,
    parameter: &str,
    generic: &str,
) -> bool {
    let mut depth: usize = 0;
    for index in start..end.saturating_sub(3) {
        match lexemes[index].text {
            "{" | "[" | "(" => depth += 1,
            "}" | "]" | ")" => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0
            && lexemes[index].is("return")
            && lexemes[index + 1].text == parameter
            && lexemes[index + 2].is("as")
            && lexemes[index + 3].text == generic
        {
            return true;
        }
    }
    false
}

fn declaration_ends_after(lexemes: &[Lexeme<'_>], index: usize) -> bool {
    lexemes
        .get(index)
        .is_none_or(|lexeme| lexeme.is(";") || lexeme.is(",") || lexeme.is("}"))
}

fn is_declaration(lexeme: Option<&Lexeme<'_>>) -> bool {
    lexeme.is_some_and(|lexeme| lexeme.is("const") || lexeme.is("let") || lexeme.is("var"))
}

fn matching_delimiter(
    lexemes: &[Lexeme<'_>],
    start: usize,
    opening: &str,
    closing: &str,
) -> Option<usize> {
    if !lexemes.get(start).is_some_and(|lexeme| lexeme.is(opening)) {
        return None;
    }

    let mut depth = 0usize;
    for (offset, lexeme) in lexemes[start..].iter().take(MAX_PATTERN_TOKENS).enumerate() {
        if lexeme.is(opening) {
            depth += 1;
        } else if lexeme.is(closing) {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

fn emit(
    diagnostics: &mut Vec<Diagnostic>,
    source_id: SourceId,
    source: &SourceText,
    rule_index: usize,
    lexeme: &Lexeme<'_>,
) {
    let (Ok(start), Ok(end)) = (
        source.byte_to_utf16(lexeme.start_byte),
        source.byte_to_utf16(lexeme.end_byte),
    ) else {
        return;
    };
    let Ok(range) = source.range(start, end) else {
        return;
    };
    let rule = &RULES[rule_index];
    let message = match rule_index {
        0 => "Method parameter bivariance can accept an incompatible callback.",
        1 => "Mutable array covariance can write an incompatible element.",
        2 => "A non-fresh object value bypasses excess-property checking.",
        3 => "Deleting a required property can violate its declared shape.",
        4 => "Catch binding property access is unchecked.",
        5 => "Casting any to a generic type bypasses its constraint.",
        6 => "Dynamic indexing can read beyond a tuple's bounds.",
        _ => unreachable!("legacy warning index is closed"),
    };
    diagnostics.push(
        Diagnostic::lint(LintLevel::Warn, rule.id(), source_id, range, message)
            .expect("legacy hard warnings are enabled"),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{analyze_hard_warnings, analyze_source_text};
    use crate::{
        parser, scanner,
        source::{ScriptKind, SourceId, SourceText},
    };

    fn codes(source: &str) -> Vec<&'static str> {
        let source = SourceText::new(source).expect("test source fits the per-file budget");
        analyze_source_text(SourceId::new(0), &source)
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    #[test]
    fn w001_detects_typed_interface_methods_but_not_function_properties() {
        assert_eq!(
            codes("interface Handler { handle(value: Animal): void; }"),
            ["BAMTS-W001"]
        );
        assert!(codes("interface Handler { handle: (value: Animal) => void; }").is_empty());
    }

    #[test]
    fn w002_detects_mismatched_named_mutable_array_assignment_but_not_matching_types() {
        assert_eq!(
            codes("const dogs: Dog[] = []; const animals: Animal[] = dogs;"),
            ["BAMTS-W002"]
        );
        assert!(codes("const dogs: Animal[] = []; const animals: Animal[] = dogs;").is_empty());
    }

    #[test]
    fn w003_detects_non_fresh_excess_property_bypass_but_not_a_fresh_object() {
        assert_eq!(
            codes(
                "const candidate = { keep: 1, extra: true }; const target: { keep: number } = candidate;"
            ),
            ["BAMTS-W003"]
        );
        assert!(codes("const target: { keep: number } = { keep: 1, extra: true };").is_empty());
    }

    #[test]
    fn w004_detects_delete_of_required_property_but_not_optional_property() {
        assert_eq!(
            codes("const item: { required: number } = { required: 1 }; delete item.required;"),
            ["BAMTS-W004"]
        );
        assert!(codes("const item: { optional?: number } = {}; delete item.optional;").is_empty());
    }

    #[test]
    fn w005_detects_direct_catch_property_access_but_not_a_guarded_access() {
        assert_eq!(
            codes("try {} catch (error) { error.message; }"),
            ["BAMTS-W005"]
        );
        assert!(
            codes("try {} catch (error) { if (error instanceof Error) error.message; }").is_empty()
        );
    }

    #[test]
    fn w006_detects_direct_generic_any_downcast_but_not_unknown() {
        assert_eq!(
            codes("function cast<T>(value: any): T { return value as T; }"),
            ["BAMTS-W006"]
        );
        assert!(codes("function cast<T>(value: unknown): T { return value as T; }").is_empty());
    }

    #[test]
    fn w007_detects_dynamic_tuple_index_but_not_a_literal_index() {
        assert_eq!(
            codes("const pair: [string, number] = [\"a\", 1]; pair[index];"),
            ["BAMTS-W007"]
        );
        assert!(codes("const pair: [string, number] = [\"a\", 1]; pair[1];").is_empty());
    }
    #[test]
    fn recognizers_ignore_comment_string_and_regex_text() {
        assert!(codes("// catch (error) { error.message; }").is_empty());
        assert!(codes("const note = \"catch (error) { error.message; }\";").is_empty());
        assert!(codes("const matcher = /catch \\(error\\) \\{ error\\.message; \\}/;").is_empty());
    }

    #[test]
    fn javascript_skips_typescript_only_hard_warnings() {
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::JavaScript,
            Arc::new(
                SourceText::new("try {} catch (error) { error.message; } value === NaN;")
                    .expect("test source fits the per-file budget"),
            ),
        ));
        let codes = analyze_hard_warnings(&parsed)
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"BAMTS-W079"));
        assert!(!codes.contains(&"BAMTS-W005"));
    }
}
