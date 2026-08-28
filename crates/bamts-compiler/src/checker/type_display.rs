use std::fmt::Write as _;

use super::{SemanticModel, SymbolId, Type, TypeId};
use bamts_bytecode::EcmaString;

/// Renders an interned type as its canonical TypeScript display string.
#[must_use]
pub fn render_type(model: &SemanticModel, type_id: TypeId) -> String {
    let mut visiting_aliases = Vec::new();
    render_type_grouped(model, type_id, false, &mut visiting_aliases)
}

fn render_type_grouped(
    model: &SemanticModel,
    type_id: TypeId,
    group: bool,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    match model.types().get(type_id) {
        Type::Error | Type::Any => "any".to_owned(),
        Type::Unknown => "unknown".to_owned(),
        Type::Never => "never".to_owned(),
        Type::Void => "void".to_owned(),
        Type::Null => "null".to_owned(),
        Type::Undefined => "undefined".to_owned(),
        Type::Boolean => "boolean".to_owned(),
        Type::Number => "number".to_owned(),
        Type::BigInt => "bigint".to_owned(),
        Type::String => "string".to_owned(),
        Type::Symbol => "symbol".to_owned(),
        Type::Object => "object".to_owned(),
        Type::BooleanLiteral(value) => if *value { "true" } else { "false" }.to_owned(),
        Type::NumberLiteral(text) | Type::BigIntLiteral(text) => text.to_string(),
        Type::StringLiteral(text) => render_string_literal(text),
        Type::Array(element) => format!(
            "{}[]",
            render_type_grouped(model, *element, true, visiting_aliases)
        ),
        Type::Tuple(shape) => {
            let mut elements = Vec::with_capacity(
                shape.prefix.len() + usize::from(shape.rest.is_some()) + shape.suffix.len(),
            );
            elements.extend(shape.prefix.iter().enumerate().map(|(index, element)| {
                let optional = index >= usize::try_from(shape.required).expect("tuple length fits");
                let rendered = render_type_grouped(model, *element, optional, visiting_aliases);
                if optional {
                    format!("{rendered}?")
                } else {
                    rendered
                }
            }));
            if let Some(rest) = shape.rest {
                elements.push(format!(
                    "...{}[]",
                    render_type_grouped(model, rest, true, visiting_aliases)
                ));
            }
            elements.extend(
                shape
                    .suffix
                    .iter()
                    .map(|element| render_type_grouped(model, *element, false, visiting_aliases)),
            );
            format!("[{}]", elements.join(", "))
        }
        Type::Union(members) => {
            let body = members
                .iter()
                .map(|member| render_type_grouped(model, *member, true, visiting_aliases))
                .collect::<Vec<_>>()
                .join(" | ");
            if group { format!("({body})") } else { body }
        }
        Type::Intersection(members) => {
            let body = members
                .iter()
                .map(|member| render_type_grouped(model, *member, true, visiting_aliases))
                .collect::<Vec<_>>()
                .join(" & ");
            if group { format!("({body})") } else { body }
        }
        Type::Function(signature) => {
            let type_params = if signature.type_parameters().is_empty() {
                String::new()
            } else {
                let names = signature
                    .type_parameters()
                    .iter()
                    .map(|symbol| model.symbol(*symbol).name().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("<{names}>")
            };
            let params = signature
                .parameters()
                .iter()
                .map(|param| {
                    format!(
                        "{}: {}",
                        param.name(),
                        render_type_grouped(model, param.type_id(), false, visiting_aliases)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let body = format!(
                "{type_params}({params}) => {}",
                render_type_grouped(model, signature.return_type(), false, visiting_aliases)
            );
            if group { format!("({body})") } else { body }
        }
        Type::ObjectType(object) => {
            if object.properties().is_empty() {
                "{}".to_owned()
            } else {
                let body = object
                    .properties()
                    .iter()
                    .map(|property| {
                        format!(
                            "{}{}{}: {}",
                            if property.readonly() { "readonly " } else { "" },
                            property.name(),
                            if property.optional() { "?" } else { "" },
                            render_type_grouped(model, property.type_id(), false, visiting_aliases)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{{ {body}; }}")
            }
        }
        Type::AppliedClass { symbol, arguments } => {
            let name = model.symbol(*symbol).name();
            if arguments.is_empty() {
                name.to_owned()
            } else {
                let arguments = arguments
                    .iter()
                    .map(|argument| render_type_grouped(model, *argument, false, visiting_aliases))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}<{arguments}>")
            }
        }
        Type::AppliedAlias { symbol, arguments } => {
            if !visiting_aliases.contains(symbol)
                && let Some(view) = model.types().applied_alias_view(type_id)
            {
                visiting_aliases.push(*symbol);
                let rendered = render_type_grouped(model, view, group, visiting_aliases);
                visiting_aliases.pop();
                rendered
            } else {
                let name = model.symbol(*symbol).name();
                if arguments.is_empty() {
                    name.to_owned()
                } else {
                    let arguments = arguments
                        .iter()
                        .map(|argument| {
                            render_type_grouped(model, *argument, false, visiting_aliases)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{name}<{arguments}>")
                }
            }
        }
        Type::Keyof(operand) => {
            let body = format!(
                "keyof {}",
                render_type_grouped(model, *operand, true, visiting_aliases)
            );
            if group { format!("({body})") } else { body }
        }
        Type::IndexedAccess { object, index } => format!(
            "{}[{}]",
            render_type_grouped(model, *object, true, visiting_aliases),
            render_type_grouped(model, *index, false, visiting_aliases)
        ),
        Type::Record { key, value } => format!(
            "Record<{}, {}>",
            render_type_grouped(model, *key, false, visiting_aliases),
            render_type_grouped(model, *value, false, visiting_aliases)
        ),
        Type::This { .. } => "this".to_owned(),
        Type::Named(symbol) | Type::NumericEnum(symbol) => model.symbol(*symbol).name().to_owned(),
    }
}

fn render_string_literal(value: &EcmaString) -> String {
    let mut out = String::with_capacity(value.len_units() + 2);
    out.push('"');
    for (_, code_point) in value.code_points() {
        match char::from_u32(code_point) {
            Some('"') => out.push_str("\\\""),
            Some('\\') => out.push_str("\\\\"),
            Some('\n') => out.push_str("\\n"),
            Some('\t') => out.push_str("\\t"),
            Some('\r') => out.push_str("\\r"),
            Some('\u{0008}') => out.push_str("\\b"),
            Some('\u{000C}') => out.push_str("\\f"),
            Some('\u{000B}') => out.push_str("\\v"),
            Some(character) if character.is_control() && code_point < 0x20 => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
            Some('\u{2028}' | '\u{2029}') => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
            Some(character) => out.push(character),
            None => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::EcmaString;

    use super::render_string_literal;

    #[test]
    fn string_literals_preserve_controls_and_unpaired_surrogates() {
        assert_eq!(
            render_string_literal(&EcmaString::encode("a\n\"b")),
            "\"a\\n\\\"b\""
        );
        assert_eq!(
            render_string_literal(&EcmaString::from_units(&[0xD800])),
            "\"\\uD800\""
        );
    }
}
