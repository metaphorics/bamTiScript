use std::fmt::Write as _;

use super::binder::{FunctionSignature, ObjectType, PropertyType};
use super::{SemanticModel, SymbolId, Type, TypeId};
use crate::literal::number_value;
use bamts_bytecode::EcmaString;
/// Renders an interned type as its canonical TypeScript display string.
#[must_use]
pub fn render_type(model: &SemanticModel, type_id: TypeId) -> String {
    let mut visiting_aliases = Vec::new();
    render_type_grouped(model, type_id, false, &mut visiting_aliases)
}

/// Renders an interned type as a declaration-emit string.
///
/// Like [`render_type`] but formats object types with multiple members across
/// multiple lines (indented by `indent` spaces), matching tsc's `.d.ts`
/// output for synthesized types. Single call/construct signatures keep the
/// inline arrow form.
#[must_use]
pub fn render_type_declaration(model: &SemanticModel, type_id: TypeId, indent: usize) -> String {
    let mut visiting_aliases = Vec::new();
    render_type_declaration_grouped(model, type_id, false, indent, &mut visiting_aliases)
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
        Type::NumberLiteral(text) => render_number_literal(text),
        Type::BigIntLiteral(text) => text.to_string(),
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
            let body = render_signature(model, signature, " => ", visiting_aliases);
            if group { format!("({body})") } else { body }
        }
        Type::ObjectType(object) => render_object_type(model, object, visiting_aliases),
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
        Type::EnumMember {
            enum_symbol,
            member_symbol,
            ..
        } => {
            let enum_name = model.symbol(*enum_symbol).name();
            let member_name = model.symbol(*member_symbol).name();
            format!("{enum_name}.{member_name}")
        }
        Type::Named(symbol) | Type::NumericEnum(symbol) => model.symbol(*symbol).name().to_owned(),
        Type::ConstructorType {
            symbol, arguments, ..
        } => {
            let name = model.symbol(*symbol).name();
            if arguments.is_empty() {
                format!("typeof {name}")
            } else {
                let arguments = arguments
                    .iter()
                    .map(|argument| render_type_grouped(model, *argument, false, visiting_aliases))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("typeof {name}<{arguments}>")
            }
        }
    }
}

fn render_type_declaration_grouped(
    model: &SemanticModel,
    type_id: TypeId,
    group: bool,
    indent: usize,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    match model.types().get(type_id) {
        Type::ObjectType(object) => {
            render_object_type_declaration(model, object, indent, visiting_aliases)
        }
        Type::Function(signature) => {
            let body =
                render_signature_declaration(model, signature, " => ", indent, visiting_aliases);
            if group { format!("({body})") } else { body }
        }
        Type::Union(members) => {
            let body = members
                .iter()
                .map(|member| {
                    render_type_declaration_grouped(model, *member, true, indent, visiting_aliases)
                })
                .collect::<Vec<_>>()
                .join(" | ");
            if group { format!("({body})") } else { body }
        }
        Type::Intersection(members) => {
            let body = members
                .iter()
                .map(|member| {
                    render_type_declaration_grouped(model, *member, true, indent, visiting_aliases)
                })
                .collect::<Vec<_>>()
                .join(" & ");
            if group { format!("({body})") } else { body }
        }
        Type::Array(element) => format!(
            "{}[]",
            render_type_declaration_grouped(model, *element, true, indent, visiting_aliases)
        ),
        Type::Tuple(shape) => {
            let mut elements = Vec::with_capacity(
                shape.prefix.len() + usize::from(shape.rest.is_some()) + shape.suffix.len(),
            );
            elements.extend(shape.prefix.iter().enumerate().map(|(index, element)| {
                let optional = index >= usize::try_from(shape.required).expect("tuple length fits");
                let rendered = render_type_declaration_grouped(
                    model,
                    *element,
                    optional,
                    indent,
                    visiting_aliases,
                );
                if optional {
                    format!("{rendered}?")
                } else {
                    rendered
                }
            }));
            if let Some(rest) = shape.rest {
                elements.push(format!(
                    "...{}[]",
                    render_type_declaration_grouped(model, rest, true, indent, visiting_aliases)
                ));
            }
            elements.extend(shape.suffix.iter().map(|element| {
                render_type_declaration_grouped(model, *element, false, indent, visiting_aliases)
            }));
            format!("[{}]", elements.join(", "))
        }
        // For all other type kinds, delegate to the inline renderer.
        _ => render_type_grouped(model, type_id, group, visiting_aliases),
    }
}

fn render_signature_declaration(
    model: &SemanticModel,
    signature: &FunctionSignature,
    return_separator: &str,
    indent: usize,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    let type_params = if signature.type_parameters().is_empty() {
        String::new()
    } else {
        let names = signature
            .type_parameters()
            .iter()
            .zip(signature.type_parameter_bounds())
            .map(|(symbol, bounds)| {
                let mut rendered = model.symbol(*symbol).name().to_owned();
                if let Some(constraint) = bounds.constraint() {
                    rendered.push_str(" extends ");
                    rendered.push_str(&render_type_declaration_grouped(
                        model,
                        constraint,
                        false,
                        indent,
                        visiting_aliases,
                    ));
                }
                if let Some(default) = bounds.default() {
                    rendered.push_str(" = ");
                    rendered.push_str(&render_type_declaration_grouped(
                        model,
                        default,
                        false,
                        indent,
                        visiting_aliases,
                    ));
                }
                rendered
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{names}>")
    };
    let params = signature
        .parameters()
        .iter()
        .map(|param| {
            format!(
                "{}{}{}: {}",
                if param.rest() { "..." } else { "" },
                param.name(),
                if param.optional() { "?" } else { "" },
                render_type_declaration_grouped(
                    model,
                    param.type_id(),
                    false,
                    indent,
                    visiting_aliases
                )
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{type_params}({params}){return_separator}{}",
        render_type_declaration_grouped(
            model,
            signature.return_type(),
            false,
            indent,
            visiting_aliases
        )
    )
}

fn render_object_type_declaration(
    model: &SemanticModel,
    object: &ObjectType,
    indent: usize,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    // Single call/construct signature with no properties or index signatures
    // stays inline (arrow form), matching tsc's declaration output.
    if object.properties.is_empty()
        && object.index_signatures.is_empty()
        && object.call_signatures.len() + object.construct_signatures.len() == 1
    {
        if let Some(signature) = object.call_signatures.first() {
            return render_signature_declaration(
                model,
                signature,
                " => ",
                indent,
                visiting_aliases,
            );
        }
        if let Some(entry) = object.construct_signatures.first() {
            return format!(
                "{}new {}",
                if entry.is_abstract { "abstract " } else { "" },
                render_signature_declaration(
                    model,
                    &entry.signature,
                    " => ",
                    indent,
                    visiting_aliases
                )
            );
        }
    }

    let inner_indent = indent + 4;
    let pad = " ".repeat(inner_indent);
    let close_pad = " ".repeat(indent);

    let mut members = Vec::with_capacity(
        object.call_signatures.len()
            + object.construct_signatures.len()
            + object.index_signatures.len()
            + object.properties.len(),
    );
    members.extend(object.call_signatures.iter().map(|signature| {
        render_signature_declaration(model, signature, ": ", inner_indent, visiting_aliases)
    }));
    members.extend(object.construct_signatures.iter().map(|entry| {
        format!(
            "{}new {}",
            if entry.is_abstract { "abstract " } else { "" },
            render_signature_declaration(
                model,
                &entry.signature,
                ": ",
                inner_indent,
                visiting_aliases
            )
        )
    }));
    members.extend(object.index_signatures.iter().map(|signature| {
        let parameters = signature
            .parameters
            .iter()
            .map(|parameter| {
                format!(
                    "{}: {}",
                    parameter.name(),
                    render_type_declaration_grouped(
                        model,
                        parameter.type_id(),
                        false,
                        inner_indent,
                        visiting_aliases
                    )
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}[{parameters}]: {}",
            if signature.readonly { "readonly " } else { "" },
            render_type_declaration_grouped(
                model,
                signature.value_type,
                false,
                inner_indent,
                visiting_aliases
            )
        )
    }));
    members.extend(object.properties.iter().map(|property| {
        render_property_declaration(model, property, inner_indent, visiting_aliases)
    }));
    if members.is_empty() {
        "{}".to_owned()
    } else {
        let body = members
            .iter()
            .map(|m| format!("{pad}{m};"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{{\n{body}\n{close_pad}}}")
    }
}

fn render_signature(
    model: &SemanticModel,
    signature: &FunctionSignature,
    return_separator: &str,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    let type_params = if signature.type_parameters().is_empty() {
        String::new()
    } else {
        let names = signature
            .type_parameters()
            .iter()
            .zip(signature.type_parameter_bounds())
            .map(|(symbol, bounds)| {
                let mut rendered = model.symbol(*symbol).name().to_owned();
                if let Some(constraint) = bounds.constraint() {
                    rendered.push_str(" extends ");
                    rendered.push_str(&render_type_grouped(
                        model,
                        constraint,
                        false,
                        visiting_aliases,
                    ));
                }
                if let Some(default) = bounds.default() {
                    rendered.push_str(" = ");
                    rendered.push_str(&render_type_grouped(
                        model,
                        default,
                        false,
                        visiting_aliases,
                    ));
                }
                rendered
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{names}>")
    };
    let params = signature
        .parameters()
        .iter()
        .map(|param| {
            format!(
                "{}{}{}: {}",
                if param.rest() { "..." } else { "" },
                param.name(),
                if param.optional() { "?" } else { "" },
                render_type_grouped(model, param.type_id(), false, visiting_aliases)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{type_params}({params}){return_separator}{}",
        render_type_grouped(model, signature.return_type(), false, visiting_aliases)
    )
}

fn render_object_type(
    model: &SemanticModel,
    object: &ObjectType,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    if object.properties.is_empty()
        && object.index_signatures.is_empty()
        && object.call_signatures.len() + object.construct_signatures.len() == 1
    {
        if let Some(signature) = object.call_signatures.first() {
            return render_signature(model, signature, " => ", visiting_aliases);
        }
        if let Some(entry) = object.construct_signatures.first() {
            return format!(
                "{}new {}",
                if entry.is_abstract { "abstract " } else { "" },
                render_signature(model, &entry.signature, " => ", visiting_aliases)
            );
        }
    }

    let mut members = Vec::with_capacity(
        object.call_signatures.len()
            + object.construct_signatures.len()
            + object.index_signatures.len()
            + object.properties.len(),
    );
    members.extend(
        object
            .call_signatures
            .iter()
            .map(|signature| render_signature(model, signature, ": ", visiting_aliases)),
    );
    members.extend(object.construct_signatures.iter().map(|entry| {
        format!(
            "{}new {}",
            if entry.is_abstract { "abstract " } else { "" },
            render_signature(model, &entry.signature, ": ", visiting_aliases)
        )
    }));
    members.extend(object.index_signatures.iter().map(|signature| {
        let parameters = signature
            .parameters
            .iter()
            .map(|parameter| {
                format!(
                    "{}: {}",
                    parameter.name(),
                    render_type_grouped(model, parameter.type_id(), false, visiting_aliases)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}[{parameters}]: {}",
            if signature.readonly { "readonly " } else { "" },
            render_type_grouped(model, signature.value_type, false, visiting_aliases)
        )
    }));
    members.extend(
        object
            .properties
            .iter()
            .map(|property| render_property(model, property, visiting_aliases)),
    );
    if members.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {}; }}", members.join("; "))
    }
}

/// Normalizes a numeric lexeme to tsc's canonical display form.
///
/// tsc renders number literal types via `ts.numberToString`, which parses
/// the value and emits the shortest decimal representation. Hex/octal/binary
/// prefixes, trailing `.0`, and integer-valued exponents all collapse to
/// the plain integer. A bare `0x` (no digits) is `NaN` in JS but tsc treats
/// it as `0` in the type position.
///
/// Baseline proof: `tests/baselines/reference/numericLiteralTypes1.types`
/// rows `>A2 : 1` (source `1.0`), `>A3 : 1` (source `1e0`), `>A4 : 1`
/// (source `10e-1`).
/// Counterexample checked: `tests/baselines/reference/literalTypes1.types`
/// row `>c2 : 100` (source `100`), confirming plain integers pass through.
fn render_number_literal(text: &str) -> String {
    match number_value(text) {
        Some(value) if value.is_finite() => {
            if value == 0.0 {
                "0".to_owned()
            } else if value == value.trunc() && value.abs() < 1e21 {
                format!("{value:.0}")
            } else {
                // Fall back to Rust's f64 Display, which matches tsc for
                // non-integer values in the common cases.
                value.to_string()
            }
        }
        _ => text.to_owned(),
    }
}

/// Renders one property of an object type, choosing method shorthand
/// (`name(params): ret`) or property form (`name: type`) to match tsc.
///
/// tsc distinguishes method declarations (`method() { }`) from function-
/// expression properties (`prop: () => void`). Methods use the
/// `name(params): ret` shorthand; properties use `name: type`.
///
/// Baseline proof: `tests/baselines/reference/objectSpreadWithinMethodWithinObjectWithSpread.types`
/// row `>a : { prop(): { metadata: number; }; }` — `prop` is a method
/// shorthand, rendered with `()` not `: () =>`.
/// Counterexample checked: same baseline row `>p1 : () => void` inside the
/// object — `p1` is a function-expression property, rendered as `name: type`.
fn render_property(
    model: &SemanticModel,
    property: &PropertyType,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    let prefix = if property.readonly() { "readonly " } else { "" };
    let optional = if property.optional() { "?" } else { "" };
    if property.is_method()
        && let Type::Function(signature) = model.types().get(property.type_id())
    {
        // Method shorthand: `name?(params): returnType` (no `=>`).
        // The optional marker sits between the name and the parameter list.
        let body = render_signature(model, signature, ": ", visiting_aliases);
        format!("{prefix}{}{optional}{body}", property.name())
    } else {
        format!(
            "{prefix}{}{optional}: {}",
            property.name(),
            render_type_grouped(model, property.type_id(), false, visiting_aliases)
        )
    }
}

/// Declaration-emit variant of [`render_property`], using the declaration
/// renderer for nested types and passing the indent context.
fn render_property_declaration(
    model: &SemanticModel,
    property: &PropertyType,
    indent: usize,
    visiting_aliases: &mut Vec<SymbolId>,
) -> String {
    let prefix = if property.readonly() { "readonly " } else { "" };
    let optional = if property.optional() { "?" } else { "" };
    if property.is_method()
        && let Type::Function(signature) = model.types().get(property.type_id())
    {
        let body = render_signature_declaration(model, signature, ": ", indent, visiting_aliases);
        format!("{prefix}{}{optional}{body}", property.name())
    } else {
        format!(
            "{prefix}{}{optional}: {}",
            property.name(),
            render_type_declaration_grouped(
                model,
                property.type_id(),
                false,
                indent,
                visiting_aliases
            )
        )
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
    use std::sync::Arc;

    use bamts_bytecode::EcmaString;

    use super::{render_string_literal, render_type};
    use crate::checker::{SemanticModel, TypeId, check};
    use crate::parser;
    use crate::scanner;
    use crate::source::{ScriptKind, SourceId, SourceText};

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

    /// Parses and checks one TypeScript source, returning the source text and
    /// the resulting semantic model.
    fn check_text(
        text: &'static str,
    ) -> (Arc<SourceText>, crate::checker::Recovered<SemanticModel>) {
        let source_text = Arc::new(SourceText::new(text).expect("test source fits"));
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::clone(&source_text),
        ));
        let checked = check(&parsed);
        (source_text, checked)
    }

    /// Returns the recorded type of the typed expression whose source slice is
    /// exactly `needle`, searched from byte offset `from`. Test sources are
    /// ASCII, so byte offsets and UTF-16 offsets coincide.
    fn typed_expression_of(
        model: &SemanticModel,
        source: &SourceText,
        needle: &str,
        from: usize,
    ) -> TypeId {
        let start = source.as_str()[from..]
            .find(needle)
            .expect("needle is present")
            + from;
        let end = start + needle.len();
        let matches: Vec<TypeId> = model
            .typed_expressions()
            .iter()
            .filter(|(range, _)| range.start().get() == start && range.end().get() == end)
            .map(|(_, type_id)| *type_id)
            .collect();
        let [type_id] = matches.as_slice() else {
            panic!("expected exactly one typed expression for {needle:?}");
        };
        *type_id
    }

    #[test]
    fn array_concat_member_renders_the_library_overload_pair() {
        // Upstream baseline: tests/baselines/reference/concatTuples.types
        // (sha256 d5e672ec6281c8d0409d3b809a4cd56882631d4030a282a4bf1e2af635337140),
        // row `>ijs.concat : {...}`.
        let text = "let ijs: [number, number][] = [[1, 2]];\nijs = ijs.concat([[3, 4], [5, 6]]);\n";
        let (source, checked) = check_text(text);
        let model = checked.product();
        let type_id = typed_expression_of(model, &source, "ijs.concat", 0);
        assert_eq!(
            render_type(model, type_id),
            "{ (...items: ConcatArray<[number, number]>[]): [number, number][]; \
             (...items: (ConcatArray<[number, number]> | [number, number])[]): [number, number][]; }"
        );
    }

    #[test]
    fn declaration_only_construct_signature_renders_new_arrow() {
        // Upstream baseline:
        // tests/baselines/reference/comparisonOperatorWithNoRelationshipObjectsOnConstructorSignature.types
        // (sha256 caaad48f518da3a8b3c6dcf3d0d94661099a825ac8db65c85dc13ec7379dcd92),
        // row `>b4 : new () => C`.
        let text = "class C { }\ndeclare var b4: { new (): C };\nlet x = b4;\n";
        let (source, checked) = check_text(text);
        let model = checked.product();
        let type_id = typed_expression_of(model, &source, "b4", text.find("let x").unwrap());
        assert_eq!(render_type(model, type_id), "new () => C");
    }

    #[test]
    fn object_types_render_call_construct_index_and_readonly_members() {
        let text = "class Local { }\ninterface Mixed {\n    readonly id: number;\n    [key: string]: unknown;\n    (a: string): void;\n    new (): Local;\n}\ndeclare var mixed: Mixed;\nlet z = mixed;\n";
        let (source, checked) = check_text(text);
        let model = checked.product();
        let type_id = typed_expression_of(model, &source, "mixed", text.find("let z").unwrap());
        assert_eq!(
            render_type(model, type_id),
            "{ (a: string): void; new (): Local; [key: string]: unknown; readonly id: number; }"
        );
    }

    #[test]
    fn method_shorthand_renders_as_call_not_arrow() {
        // Upstream baseline:
        // tests/baselines/reference/objectSpreadWithinMethodWithinObjectWithSpread.types
        // row `>a : { prop(): { metadata: number; }; }` — method shorthand
        // `prop()` renders with `()` not `: () =>`.
        // Counterexample checked:
        // tests/baselines/reference/superInObjectLiterals_ES5(target=es2015).types
        // row `>obj : { ... p1: () => void; ... }` — function-expression
        // properties keep `name: () => type` form.
        let text = "interface I { prop(): { metadata: number; }; }\ndeclare var a: I;\nlet z = a;\n";
        let (source, checked) = check_text(text);
        let model = checked.product();
        let type_id = typed_expression_of(model, &source, "a", text.find("let z").unwrap());
        assert_eq!(render_type(model, type_id), "{ prop(): { metadata: number; }; }");
    }

    #[test]
    fn number_literal_normalizes_hex_and_decimal_forms() {
        // Upstream baseline:
        // tests/baselines/reference/numericLiteralTypes1.types
        // rows `>A2 : 1` (source `1.0`), `>A3 : 1` (source `1e0`),
        // `>A4 : 1` (source `10e-1`) — all normalize to `1`.
        // Counterexample checked:
        // tests/baselines/reference/scannerS7.8.3_A6.1_T1.types
        // row `>0x : 0` — bare `0x` normalizes to `0`.
        // `const` preserves the literal type (unlike `let` which widens).
        let text = "const a = 1.0;\nconst b = 1e0;\nconst c = 10e-1;\nlet z = a;\nlet y = b;\nlet w = c;\n";
        let (source, checked) = check_text(text);
        let model = checked.product();
        let a = typed_expression_of(model, &source, "a", text.find("let z").unwrap());
        assert_eq!(render_type(model, a), "1");
        let b = typed_expression_of(model, &source, "b", text.find("let y").unwrap());
        assert_eq!(render_type(model, b), "1");
        let c = typed_expression_of(model, &source, "c", text.find("let w").unwrap());
        assert_eq!(render_type(model, c), "1");
    }

}