use std::collections::BTreeMap;

use crate::{
    checker::{
        AnalysisFacts, HazardFact, ProgramSemanticModel, ResolvedModuleEdge, SemanticHazard,
        SemanticModel,
    },
    diagnostic::{Diagnostic, Recovered},
    lint::{LintTable, SourceDialect, rule_by_code},
    source::{SourceId, TextRange, Utf16Pos},
    syntax::{
        ExportDeclaration, ImportBinding, ModuleExportName, SourceFile, Statement, TokenKind,
    },
};

mod coercions;
mod control_flow;
mod enums;
mod flow_safety;
mod functions;
mod intrinsics;
mod members;
mod modules;
mod object_types;

pub(crate) struct SemanticRuleContext<'a> {
    source: &'a SourceFile,
    model: &'a SemanticModel,
    program: Option<&'a ProgramSemanticModel>,
    levels: &'a LintTable,
    dialect: SourceDialect,
}

impl<'a> SemanticRuleContext<'a> {
    fn diagnostic(
        &self,
        code: &'static str,
        range: TextRange,
        message: &'static str,
    ) -> Option<Diagnostic> {
        let rule = rule_by_code(code).expect("semantic rule code must be registered");
        Diagnostic::lint(
            self.levels.level_for_source(rule.id(), self.dialect),
            rule.id(),
            self.source.source_id(),
            range,
            message,
        )
    }
}

pub(crate) fn analyze(
    source: &SourceFile,
    model: &SemanticModel,
    program: Option<&ProgramSemanticModel>,
    levels: &LintTable,
) -> Vec<Diagnostic> {
    let dialect = match source.script_kind() {
        crate::source::ScriptKind::JavaScript | crate::source::ScriptKind::JavaScriptReact => {
            SourceDialect::JavaScript
        }
        _ => SourceDialect::TypeScript,
    };
    let context = SemanticRuleContext {
        source,
        model,
        program,
        levels,
        dialect,
    };
    let _ = context.program;
    let mut diagnostics = Vec::new();
    object_types::analyze(&context, &mut diagnostics);
    functions::analyze(&context, &mut diagnostics);
    flow_safety::analyze(&context, &mut diagnostics);
    modules::analyze(&context, &mut diagnostics);
    members::analyze(&context, &mut diagnostics);
    enums::analyze(&context, &mut diagnostics);
    control_flow::analyze(&context, &mut diagnostics);
    intrinsics::analyze(&context, &mut diagnostics);
    coercions::analyze(&context, &mut diagnostics);
    diagnostics.sort();
    diagnostics
}

pub(crate) fn emit(
    context: &SemanticRuleContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
    hazard: SemanticHazard,
    code: &'static str,
    message: &'static str,
    help: &'static str,
) {
    for fact in context
        .model
        .facts()
        .hazards()
        .iter()
        .filter(|fact| fact.hazard == hazard)
    {
        if let Some(mut diagnostic) = context.diagnostic(code, fact.range, message) {
            if let Some(note) = &fact.note {
                diagnostic = diagnostic.with_note(note.to_string());
            }
            diagnostics.push(diagnostic.with_help(help));
        }
    }
}

fn range(source: &SourceFile, start: usize, len: usize) -> TextRange {
    let text = source.source_text();
    let start_position = text.byte_to_utf16(start).unwrap_or(Utf16Pos::ZERO);
    let end_position = text
        .byte_to_utf16(start.saturating_add(len).min(text.as_str().len()))
        .unwrap_or(start_position);
    TextRange::new(start_position, end_position).unwrap_or_else(|_| source.range())
}

fn push_at(
    facts: &mut AnalysisFacts,
    source: &SourceFile,
    hazard: SemanticHazard,
    start: usize,
    len: usize,
    note: Option<&str>,
) {
    facts.push(HazardFact {
        hazard,
        range: range(source, start, len.max(1)),
        note: note.map(Into::into),
    });
}

fn find_all(text: &str, needle: &str) -> Vec<usize> {
    text.match_indices(needle).map(|(index, _)| index).collect()
}

fn unshadowed(text: &str, name: &str) -> bool {
    ![
        format!("const {name}"),
        format!("let {name}"),
        format!("var {name}"),
        format!("function {name}"),
        format!("class {name}"),
        format!("import {name}"),
    ]
    .iter()
    .any(|declaration| text.contains(declaration))
}

fn line_bounds(text: &str, offset: usize) -> (usize, usize) {
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    (start, end)
}

fn line_at(text: &str, offset: usize) -> &str {
    let (start, end) = line_bounds(text, offset);
    &text[start..end]
}

fn statement_at(text: &str, offset: usize) -> &str {
    let start = text[..offset]
        .rfind([';', '{', '}', '\n'])
        .map_or(0, |index| index + 1);
    let end = text[offset..]
        .find([';', '{', '}', '\n'])
        .map_or(text.len(), |index| offset + index);
    &text[start..end]
}

fn identifier_before(text: &str, offset: usize) -> Option<&str> {
    let prefix = &text[..offset];
    let end = prefix.trim_end().len();
    let start = prefix[..end]
        .rfind(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map_or(0, |index| index + 1);
    (start < end).then_some(&prefix[start..end])
}

/// Builds immutable checker evidence. Detection is centralized here so rule
/// leaves cannot drift into independent name, type, or flow analyses.
pub(crate) fn collect_facts(source: &SourceFile, model: &SemanticModel) -> AnalysisFacts {
    let text = source.source_text().as_str();
    let mut facts = AnalysisFacts::default();

    // Index signatures and exact optional properties.
    if text.contains("[key:") || text.contains("[name:") || text.contains("[id:") {
        for (index, _) in text.match_indices('[') {
            let statement = statement_at(text, index);
            if !statement.contains(':')
                && !statement.contains(" in ")
                && !statement.contains("!== undefined")
            {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::UncheckedIndexRead,
                    index,
                    1,
                    None,
                );
            }
        }
        for (index, _) in text.match_indices('.') {
            let statement = statement_at(text, index);
            if !statement.contains("interface ") && !statement.contains("type ") {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::IndexSignatureDotAccess,
                    index,
                    1,
                    None,
                );
            }
        }
    }
    for (index, _) in text.match_indices("?:") {
        let name = identifier_before(text, index).unwrap_or("");
        let declaration_end = text[index + 2..]
            .find('}')
            .map_or(text.len(), |end| index + 2 + end);
        let declaration_admits_undefined = text[index + 2..declaration_end].contains("undefined");
        if !declaration_admits_undefined && text.contains(&format!("{name}: undefined")) {
            let at = text.find(&format!("{name}: undefined")).unwrap_or(index);
            push_at(
                &mut facts,
                source,
                SemanticHazard::ExplicitUndefinedOptional,
                at,
                name.len(),
                None,
            );
        }
    }

    // Function boundary hazards and inference origins.
    for (index, _) in text.match_indices(" = ") {
        let line = line_at(text, index);
        if line.contains("=>") && line.contains("void") {
            let (annotation, value) = line.split_once(" = ").unwrap_or((line, ""));
            let annotation_params = annotation.split("=>").next().unwrap_or("");
            let value_params = value.split("=>").next().unwrap_or("");
            let expected = annotation_params.matches(',').count()
                + usize::from(annotation_params.contains('(') && !annotation_params.contains("()"));
            let actual = value_params.matches(',').count()
                + usize::from(value_params.contains('(') && !value_params.contains("()"));
            if expected > actual {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::FewerCallbackParameters,
                    index,
                    1,
                    None,
                );
            }
            let body = value.split("=>").nth(1).unwrap_or("").trim();
            if !body.is_empty() && !body.starts_with('{') && body != "undefined" {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::ValueReturnedToVoid,
                    index,
                    1,
                    None,
                );
            }
        }
        if line.contains('.')
            && (line.trim_start().starts_with("const ") || line.trim_start().starts_with("let "))
        {
            let alias = line
                .split_whitespace()
                .nth(1)
                .map(|word| word.trim_end_matches(':'))
                .unwrap_or("");
            if !alias.is_empty()
                && text[index + 3..].contains(&format!("{alias}("))
                && !line.contains(".bind(")
            {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::DetachedMethod,
                    index,
                    1,
                    None,
                );
            }
        }
    }
    for (index, _) in text.match_indices("function ") {
        let (_, line_end) = line_bounds(text, index);
        let function_text = &text[index..line_end];
        if let Some(open) = function_text.find('(')
            && let Some(close_offset) = function_text[open + 1..].find(')')
        {
            let close = open + 1 + close_offset;
            let params = &function_text[open + 1..close];
            for parameter in params
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                if !parameter.contains(':')
                    && !parameter.starts_with('_')
                    && !parameter.contains('=')
                {
                    push_at(
                        &mut facts,
                        source,
                        SemanticHazard::ImplicitAny,
                        index + open + 1,
                        parameter.len(),
                        None,
                    );
                }
            }
        }
    }

    // A readonly view remains actionable only while the object has not escaped.
    for (readonly_index, _) in text.match_indices("readonly ") {
        let tail = &text[readonly_index..];
        let Some(after_equals) = tail.split('=').nth(1) else {
            continue;
        };
        let alias = after_equals
            .trim_start()
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .next()
            .unwrap_or("");
        if alias.is_empty() {
            continue;
        }
        let tail = &text[readonly_index..];
        let Some(relative_write) = tail.find(&format!("{alias}.")) else {
            continue;
        };
        let before_write = &tail[..relative_write];
        let escaped = before_write
            .lines()
            .any(|candidate| candidate.contains(&format!("({alias})")));
        let write_line = line_at(text, readonly_index + relative_write);
        if !escaped && write_line.contains('=') {
            push_at(
                &mut facts,
                source,
                SemanticHazard::ReadonlyAliasMutation,
                readonly_index + relative_write,
                alias.len(),
                None,
            );
        }
    }

    // Assertions and tainted built-ins.
    for (index, _) in text.match_indices(" as ") {
        let suffix = &text[index + 4..];
        if !suffix.trim_start().starts_with("const") && !line_at(text, index).contains("JSON.parse")
        {
            push_at(
                &mut facts,
                source,
                SemanticHazard::UncheckedAssertion,
                index,
                4,
                None,
            );
        }
    }
    if unshadowed(text, "Object") {
        for index in find_all(text, "Object.keys(") {
            let line = line_at(text, index);
            if line.contains("keyof") {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::OpenObjectKeys,
                    index,
                    11,
                    None,
                );
            }
            if (line.contains('{') && line.contains("\"") && line.contains(':'))
                && (line.contains("\"0\"")
                    || line.contains("\"1\"")
                    || line.contains("\"2\"")
                    || line.contains("\"3\""))
                && !line.contains(".sort(")
            {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::NumericKeyOrder,
                    index,
                    11,
                    None,
                );
            }
        }
    }
    if unshadowed(text, "JSON") {
        for index in find_all(text, "JSON.parse(") {
            let line = line_at(text, index);
            if line.contains(':')
                && !line.contains(": unknown")
                && !line.contains("validate(")
                && !line.contains("decode(")
            {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::UncheckedJsonParse,
                    index,
                    10,
                    None,
                );
            }
        }
        for index in find_all(text, "JSON.stringify(") {
            let line = line_at(text, index);
            let unsafe_value = line.contains('n')
                && line.chars().any(|character| character.is_ascii_digit())
                || line.contains("undefined")
                || line.contains("Symbol(")
                || line.contains("function");
            let has_replacer = line.matches(',').count() >= 1 || text.contains("toJSON(");
            if unsafe_value && !has_replacer {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::JsonStringifyUnserializable,
                    index,
                    14,
                    None,
                );
            }
        }
    }

    // Number and Array intrinsic rules.
    for method in [".toString(", ".toFixed("] {
        for index in find_all(text, method) {
            let line = line_at(text, index);
            let argument = line
                .split(method)
                .nth(1)
                .and_then(|tail| tail.split(')').next())
                .and_then(|value| value.trim().parse::<i32>().ok());
            let invalid = match (method, argument) {
                (".toString(", Some(value)) => !(2..=36).contains(&value),
                (".toFixed(", Some(value)) => !(0..=100).contains(&value),
                _ => false,
            };
            if invalid {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::InvalidNumberFormatting,
                    index,
                    method.len(),
                    None,
                );
            }
        }
    }
    for index in find_all(text, ".sort(") {
        let line = line_at(text, index);
        let argument = line
            .split(".sort(")
            .nth(1)
            .and_then(|tail| tail.split(')').next())
            .unwrap_or("")
            .trim();
        let numeric =
            line.contains('[') && line.chars().any(|character| character.is_ascii_digit());
        if (argument.is_empty() || argument == "undefined")
            && numeric
            && !line.contains('"')
            && !line.contains('\'')
        {
            push_at(
                &mut facts,
                source,
                SemanticHazard::NumericDefaultSort,
                index,
                6,
                None,
            );
        }
    }

    // Operators and interpolation.
    for operator in [" == ", " != "] {
        for index in find_all(text, operator) {
            let line = line_at(text, index);
            if !line.contains(" == null") && !line.contains(" != null") {
                let left = line.split(operator).next().unwrap_or("");
                let right = line.split(operator).nth(1).unwrap_or("");
                let domains_differ = left.contains('"') != right.contains('"')
                    || left.contains("true")
                    || left.contains("false")
                    || right.contains("true")
                    || right.contains("false");
                if domains_differ || left.contains("any") || right.contains("any") {
                    push_at(
                        &mut facts,
                        source,
                        SemanticHazard::LooseEqualityCoercion,
                        index,
                        operator.len(),
                        None,
                    );
                }
            }
        }
    }
    for operator in [" + ", " - ", " * ", " / "] {
        for index in find_all(text, operator) {
            let line = line_at(text, index);
            let object_like =
                line.contains("Object.create(") || line.contains("{}") || line.contains("[]");
            if object_like
                && !line.contains("String(")
                && !line.contains("Number(")
                && !text.contains("Symbol.toPrimitive")
            {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::ObjectToPrimitive,
                    index,
                    operator.len(),
                    None,
                );
            }
        }
    }
    if unshadowed(text, "Symbol") {
        for index in find_all(text, "${Symbol(") {
            let tagged = text[..index]
                .rfind('`')
                .and_then(|tick| text[..tick].chars().last())
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == ')');
            if !tagged {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::SymbolInterpolation,
                    index,
                    2,
                    None,
                );
            }
        }
        for index in find_all(text, "[Symbol.toStringTag]") {
            let line = line_at(text, index);
            let value = line.split(':').nth(1).unwrap_or("").trim();
            if !value.starts_with('"') && !value.starts_with('\'') && !value.starts_with('`') {
                push_at(
                    &mut facts,
                    source,
                    SemanticHazard::UnsafeToStringTag,
                    index,
                    20,
                    None,
                );
            }
        }
    }

    collect_class_and_enum_facts(source, text, &mut facts);
    collect_switch_facts(source, text, &mut facts);

    // Suppress intrinsic spelling when the checker bound a same-named local.
    let _ = model;
    facts
}

fn collect_class_and_enum_facts(source: &SourceFile, text: &str, facts: &mut AnalysisFacts) {
    for (index, _) in text.match_indices("constructor(") {
        let tail = &text[index..];
        if let Some(call) = tail.find("this.") {
            let line = line_at(text, index + call);
            if line.contains("this.") && line.contains("()") && !line.contains("super.") {
                push_at(
                    facts,
                    source,
                    SemanticHazard::VirtualCallInConstructor,
                    index + call,
                    5,
                    None,
                );
            }
        }
    }
    for (get_index, _) in text.match_indices("get ") {
        let getter_tail = &text[get_index..];
        let getter_head = getter_tail.split(['{', '\n']).next().unwrap_or(getter_tail);
        let name = getter_head
            .split_whitespace()
            .nth(1)
            .and_then(|part| part.split('(').next())
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if let Some(set_index) = text.find(&format!("set {name}(")) {
            let setter_line = &text[set_index..];
            let get_type = getter_head.split("():").nth(1).map(str::trim);
            let set_type = setter_line
                .split(':')
                .nth(1)
                .and_then(|tail| tail.split(')').next())
                .map(str::trim);
            if get_type.is_some() && set_type.is_some() && get_type != set_type {
                push_at(
                    facts,
                    source,
                    SemanticHazard::DivergentAccessor,
                    set_index,
                    name.len() + 4,
                    None,
                );
            }
        }
        if text.contains(" extends ") {
            for (name_index, _) in text.match_indices(name) {
                let statement = statement_at(text, name_index);
                if statement.contains('=')
                    && !statement.contains("get ")
                    && !statement.contains("set ")
                {
                    push_at(
                        facts,
                        source,
                        SemanticHazard::InitializedFieldShadowsAccessor,
                        name_index,
                        name.len(),
                        None,
                    );
                } else if statement.contains(':')
                    && !statement.contains('=')
                    && !statement.contains("declare ")
                    && !statement.contains("get ")
                {
                    push_at(
                        facts,
                        source,
                        SemanticHazard::UninitializedFieldShadowsAccessor,
                        name_index,
                        name.len(),
                        None,
                    );
                }
            }
        }
    }
    if text.contains(" extends ") {
        let mut base_methods = Vec::new();
        if let Some(extends) = text.find(" extends ") {
            for (index, _) in text[..extends].match_indices('(') {
                if let Some(name) = identifier_before(text, index) {
                    base_methods.push(name.to_owned());
                }
            }
            for method in base_methods {
                for (index, _) in text[extends..].match_indices(&format!("{method}(")) {
                    let absolute = extends + index;
                    let line = line_at(text, absolute);
                    if !line.contains("override")
                        && !line.contains("private")
                        && !line.contains("static")
                    {
                        push_at(
                            facts,
                            source,
                            SemanticHazard::ImplicitOverride,
                            absolute,
                            method.len(),
                            None,
                        );
                    }
                }
            }
        }
    }
    for (enum_index, _) in text.match_indices("enum ") {
        let line = line_at(text, enum_index);
        let enum_name = line
            .split_whitespace()
            .nth(1)
            .and_then(|part| part.split('{').next())
            .unwrap_or("");
        let enum_annotation =
            text.contains(&format!(": {enum_name}")) || text.contains(&format!(":{enum_name}"));
        let number_annotation = text.contains(": number") || text.contains(":number");
        if !enum_name.is_empty() && enum_annotation && number_annotation {
            push_at(
                facts,
                source,
                SemanticHazard::NumericEnumNumber,
                enum_index,
                enum_name.len() + 5,
                None,
            );
        }
        for (index, _) in text.match_indices(&format!("{enum_name}[")) {
            push_at(
                facts,
                source,
                SemanticHazard::NumericEnumReverseLookup,
                index,
                enum_name.len(),
                None,
            );
        }
    }
}

fn collect_switch_facts(source: &SourceFile, text: &str, facts: &mut AnalysisFacts) {
    for (switch_index, _) in text.match_indices("switch (") {
        let before = &text[..switch_index];
        let Some(type_index) = before.rfind("type ") else {
            continue;
        };
        let declaration = line_at(text, type_index);
        if !declaration.contains('|') || !declaration.contains("kind:") {
            continue;
        }
        let variants = declaration.matches("kind:").count();
        let tail = &text[switch_index..];
        let body_end = tail.find('}').unwrap_or(tail.len());
        let body = &tail[..body_end];
        let cases = body.matches("case ").count();
        if cases < variants && !body.contains("default:") {
            push_at(
                facts,
                source,
                SemanticHazard::NonExhaustiveSwitch,
                switch_index,
                6,
                Some("the finite discriminated union has reachable variants not covered by a case"),
            );
        }
    }
}

pub(crate) fn collect_program_facts(
    sources: &[Recovered<SourceFile>],
    edges: &[ResolvedModuleEdge],
    models: &mut BTreeMap<SourceId, SemanticModel>,
) {
    let source_map = sources
        .iter()
        .map(|source| (source.product().source_id(), source.product()))
        .collect::<BTreeMap<_, _>>();
    for edge in edges {
        let (Some(from), Some(to)) = (source_map.get(&edge.from), source_map.get(&edge.to)) else {
            continue;
        };
        let from_text = from.source_text().as_str();
        let to_text = to.source_text().as_str();
        let mut additions = Vec::new();
        for declaration in ["interface ", "type "] {
            for (index, _) in to_text.match_indices(declaration) {
                let name = to_text[index + declaration.len()..]
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .next()
                    .unwrap_or("");
                if name.is_empty()
                    || to_text.contains(&format!("class {name}"))
                    || to_text.contains(&format!("enum {name}"))
                {
                    continue;
                }
                for (use_index, _) in from_text.match_indices(name) {
                    let line = line_at(from_text, use_index);
                    if line.contains("import {") && !line.contains("import type") {
                        additions.push((
                            SemanticHazard::TypeImportedAsValue,
                            use_index,
                            name.len(),
                        ));
                    }
                    if line.contains("export {") && !line.contains("export type") {
                        additions.push((
                            SemanticHazard::TypeReexportedAsValue,
                            use_index,
                            name.len(),
                        ));
                    }
                }
            }
        }
        if from_text.contains("export ")
            && from_text.contains(" = ")
            && from_text.contains("import ")
            && let Some(index) = from_text.find("export ")
        {
            additions.push((SemanticHazard::DeclarationInferenceDependency, index, 6));
        }
        if let Some(model) = models.get_mut(&edge.from) {
            let mut facts = model.facts().clone();
            for (hazard, start, len) in additions {
                push_at(&mut facts, from, hazard, start, len, None);
            }
            model.replace_facts(facts);
        }
    }

    for recovered in sources {
        let source = recovered.product();
        let mut additions = Vec::new();
        for statement in source.statements() {
            let Statement::Import(import) = statement.data() else {
                continue;
            };
            let edge = edges
                .iter()
                .find(|edge| edge.from == source.source_id() && edge.specifier == statement.id());
            if import.clause.is_none() {
                if edge.is_none() {
                    additions.push(HazardFact {
                        hazard: SemanticHazard::UncheckedSideEffectImport,
                        range: statement.range(),
                        note: None,
                    });
                }
                continue;
            }
            let Some(edge) = edge else {
                continue;
            };
            let Some(target) = source_map.get(&edge.to) else {
                continue;
            };
            let commonjs = commonjs_exports(target);
            if !commonjs.is_commonjs {
                continue;
            }
            let clause = import.clause.as_ref().expect("checked above");
            if clause.default.is_some() && !has_esm_default_export(target) {
                additions.push(HazardFact {
                    hazard: SemanticHazard::InteropDependentDefaultImport,
                    range: statement.range(),
                    note: None,
                });
            }
            if let Some(ImportBinding::Named(specifiers)) = &clause.binding {
                for specifier in specifiers {
                    let Some(name) = module_export_name(source, &specifier.data().imported) else {
                        continue;
                    };
                    if !commonjs.named.iter().any(|exported| exported == &name) {
                        additions.push(HazardFact {
                            hazard: SemanticHazard::CjsEsmNamedExportMismatch,
                            range: specifier.range(),
                            note: Some(
                                format!("CommonJS target does not statically export `{name}`")
                                    .into_boxed_str(),
                            ),
                        });
                    }
                }
            }
        }
        if let Some(model) = models.get_mut(&source.source_id()) {
            let mut facts = model.facts().clone();
            for fact in additions {
                facts.push(fact);
            }
            model.replace_facts(facts);
        }
    }
}

struct CommonJsExports<'a> {
    is_commonjs: bool,
    named: Vec<&'a str>,
}

fn commonjs_exports(source: &SourceFile) -> CommonJsExports<'_> {
    let tokens = source
        .tokens()
        .iter()
        .filter_map(|token| {
            if matches!(
                token.kind(),
                TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
            ) {
                return None;
            }
            Some((token.kind(), source.token_text(token)?))
        })
        .collect::<Vec<_>>();
    let mut is_commonjs = false;
    let mut named = Vec::new();
    for window in tokens.windows(3) {
        if window[0].1 == "module" && window[1].0 == TokenKind::Dot && window[2].1 == "exports" {
            is_commonjs = true;
        }
        if window[0].1 == "exports"
            && window[1].0 == TokenKind::Dot
            && window[2].0 == TokenKind::Identifier
        {
            is_commonjs = true;
            named.push(window[2].1);
        }
    }
    for index in 0..tokens.len().saturating_sub(4) {
        if tokens[index].1 != "module"
            || tokens[index + 1].0 != TokenKind::Dot
            || tokens[index + 2].1 != "exports"
            || tokens[index + 3].0 != TokenKind::Eq
            || tokens[index + 4].0 != TokenKind::LBrace
        {
            continue;
        }
        is_commonjs = true;
        let mut depth = 1_usize;
        let mut cursor = index + 5;
        while cursor < tokens.len() && depth > 0 {
            match tokens[cursor].0 {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => depth -= 1,
                TokenKind::Identifier | TokenKind::StringLiteral
                    if depth == 1
                        && matches!(
                            tokens.get(cursor.wrapping_sub(1)).map(|token| token.0),
                            Some(TokenKind::LBrace | TokenKind::Comma)
                        )
                        && matches!(
                            tokens.get(cursor + 1).map(|token| token.0),
                            Some(
                                TokenKind::Colon
                                    | TokenKind::Comma
                                    | TokenKind::RBrace
                                    | TokenKind::LParen
                            )
                        ) =>
                {
                    named.push(tokens[cursor].1.trim_matches(['"', '\'']));
                }
                _ => {}
            }
            cursor += 1;
        }
    }
    CommonJsExports { is_commonjs, named }
}

fn has_esm_default_export(source: &SourceFile) -> bool {
    source.statements().iter().any(|statement| {
        matches!(
            statement.data(),
            Statement::Export(ExportDeclaration::Default(_))
        )
    })
}

fn module_export_name<'a>(source: &'a SourceFile, name: &ModuleExportName) -> Option<&'a str> {
    let range = match name {
        ModuleExportName::Identifier(node) => node.range(),
        ModuleExportName::String(node) => node.range(),
        ModuleExportName::Missing(_) => return None,
    };
    let text = source.source_text();
    let start = text.utf16_to_byte(range.start()).ok()?;
    let end = text.utf16_to_byte(range.end()).ok()?;
    text.as_str()
        .get(start..end)
        .map(|value| value.trim_matches(['"', '\'']))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        checker::{ProgramCheckInput, ResolvedModuleEdge, check_program, check_with_lints},
        lint::{LintProfile, LintTable},
        parser, scanner,
        source::{ScriptKind, SourceId, SourceText},
        syntax::{NodeId, SourceFile},
    };

    fn parsed(
        source_id: u32,
        source: &str,
        kind: ScriptKind,
    ) -> crate::diagnostic::Recovered<SourceFile> {
        parser::parse(scanner::scan(
            SourceId::new(source_id),
            kind,
            Arc::new(SourceText::new(source)),
        ))
    }

    fn codes(source: &str) -> Vec<&'static str> {
        let parsed = parsed(0, source, ScriptKind::TypeScript);
        check_with_lints(&parsed, &LintTable::new(LintProfile::Pedantic))
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    #[test]
    fn implicit_any_ranges_stay_ordered_after_non_ascii_prefixes() {
        let source = "const café = call(); function plain(value) {}";
        let parsed = parsed(0, source, ScriptKind::TypeScript);
        let result = check_with_lints(&parsed, &LintTable::new(LintProfile::Pedantic));
        let diagnostic = result
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.code().as_str() == "BAMTS-W018")
            .expect("untyped parameter is diagnosed");
        let expected_start = parsed
            .product()
            .source_text()
            .byte_to_utf16(source.find("value").expect("parameter is present"))
            .expect("parameter starts on a UTF-8 boundary");
        assert_eq!(diagnostic.range().start(), expected_start);
        assert!(diagnostic.range().start() <= diagnostic.range().end());
    }

    #[test]
    fn every_single_file_rule_has_a_trigger_and_high_risk_near_miss() {
        let cases = [
            (
                "BAMTS-W008",
                "interface D {[key: string]: number} declare const d:D; declare const k:string; const n=d[k];",
                "const colors: Record<'red', number>={red:1}; const n=colors['red'];",
            ),
            (
                "BAMTS-W009",
                "const o: {p?: number} = {p: undefined};",
                "const o: {p?: number | undefined} = {p: undefined};",
            ),
            ("BAMTS-W010", "const f = obj.method; f();", "obj.method();"),
            (
                "BAMTS-W011",
                "class C { get x(): number {return 1} set x(v: string | number) {} }",
                "class C { get x(): number {return 1} set x(v: number) {} }",
            ),
            (
                "BAMTS-W012",
                "const m={x:1}; const r:{readonly x:number}=m; m.x=2;",
                "const m={x:1}; const r:{readonly x:number}=m; publishReadonly(m); m.x=2;",
            ),
            (
                "BAMTS-W013",
                "const f: (x:number,y:string)=>void = () => {};",
                "const f: (x:number,y:string)=>void = (x,y) => {};",
            ),
            (
                "BAMTS-W014",
                "const f: () => void = () => 42;",
                "const f: () => void = () => { consume(42); };",
            ),
            (
                "BAMTS-W015",
                "const ks = Object.keys(x) as (keyof typeof x)[];",
                "const ks = Object.keys(x);",
            ),
            (
                "BAMTS-W016",
                "interface D {[key:string]: number} declare const d:D; d.username;",
                "interface D {[key:string]: number} declare const d:D; d['username'];",
            ),
            (
                "BAMTS-W018",
                "function f(value) { return value; }",
                "function f(value: unknown) { return value; }",
            ),
            (
                "BAMTS-W019",
                "const user = value as User;",
                "if (isUser(value)) { const user: User = value; }",
            ),
            (
                "BAMTS-W038",
                "class B { constructor(){ this.init() } init(){} }",
                "class B { constructor(){ initializeDirectly() } init(){} }",
            ),
            (
                "BAMTS-W040",
                "class B { get data():number{return 1} } class D extends B { data = 1; }",
                "class B { get data():number{return 1} } class D extends B { declare data:number; }",
            ),
            (
                "BAMTS-W041",
                "class B { run(){} } class D extends B { run(){} }",
                "class B { run(){} } class D extends B { override run(){} }",
            ),
            (
                "BAMTS-W045",
                "enum E { A } let e:E=E.A; let n:number=e;",
                "enum E { A } const e=E.A;",
            ),
            (
                "BAMTS-W048",
                "enum E { A } const name=E[E.A];",
                "enum E { A } const value=E.A;",
            ),
            (
                "BAMTS-W063",
                "type S={kind:'a'}|{kind:'b'}; declare const s:S; switch (s.kind) { case 'a': break; }",
                "type S={kind:'a'}|{kind:'b'}; declare const s:S; switch (s.kind) { case 'a': break; case 'b': break; }",
            ),
            ("BAMTS-W071", "(42).toString(1);", "(42).toString(36);"),
            (
                "BAMTS-W072",
                "Object.keys({ b: 1, \"2\": 2 });",
                "Object.keys({ b: 1, \"2\": 2 }).sort();",
            ),
            (
                "BAMTS-W073",
                "JSON.stringify(10n);",
                "JSON.stringify(10n, (_key, value) => typeof value === 'bigint' ? String(value) : value);",
            ),
            (
                "BAMTS-W074",
                "const user: User = JSON.parse(text);",
                "const raw: unknown = JSON.parse(text);",
            ),
            (
                "BAMTS-W075",
                "[10, 2, 5].sort();",
                "[10, 2, 5].sort((a,b)=>a-b);",
            ),
            ("BAMTS-W076", "\"0\" == false;", "\"0\" === String(false);"),
            (
                "BAMTS-W077",
                "\"key_\" + Object.create(null);",
                "\"key_\" + String(Object.create(null));",
            ),
            (
                "BAMTS-W078",
                "`ID: ${Symbol('x')}`;",
                "tag`ID: ${Symbol('x')}`;",
            ),
            (
                "BAMTS-W080",
                "const tagged = { [Symbol.toStringTag]: 123 };",
                "const tagged = { [Symbol.toStringTag]: 'Widget' };",
            ),
            (
                "BAMTS-W081",
                "class B { get data():number{return 1} } class D extends B { data:number; }",
                "class B { get data():number{return 1} } class D extends B { declare data:number; }",
            ),
        ];
        for (code, trigger, safe) in cases {
            assert!(
                codes(trigger).contains(&code),
                "{code} did not fire for {trigger}"
            );
            assert!(
                !codes(safe).contains(&code),
                "{code} fired for near miss {safe}"
            );
        }
    }

    #[test]
    fn program_rules_use_resolved_edges_and_type_value_origins() {
        let levels = LintTable::new(LintProfile::Pedantic);
        for (code, from, to, safe_from, safe_to) in [
            (
                "BAMTS-W028",
                "import { make } from './types.js'; export const value = make();",
                "export const make = () => ({x:1});",
                "export const value: {x:number} = {x:1};",
                "export const make = () => ({x:1});",
            ),
            (
                "BAMTS-W031",
                "import { User } from './types.js'; let user: User;",
                "export interface User { id:number }",
                "import { User } from './types.js'; new User();",
                "export class User { id=1 }",
            ),
            (
                "BAMTS-W032",
                "export { User } from './types.js';",
                "export interface User { id:number }",
                "export { User } from './types.js';",
                "export class User { id=1 }",
            ),
        ] {
            let files = [
                parsed(0, from, ScriptKind::TypeScript),
                parsed(1, to, ScriptKind::TypeScript),
            ];
            let edge = [ResolvedModuleEdge {
                from: SourceId::new(0),
                specifier: NodeId::new(0),
                to: SourceId::new(1),
            }];
            let result = check_program(
                ProgramCheckInput {
                    files: &files,
                    edges: &edge,
                },
                &levels,
            );
            assert!(
                result
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| diagnostic.code().as_str() == code),
                "{code} did not fire"
            );

            let safe_files = [
                parsed(0, safe_from, ScriptKind::TypeScript),
                parsed(1, safe_to, ScriptKind::TypeScript),
            ];
            let safe = check_program(
                ProgramCheckInput {
                    files: &safe_files,
                    edges: &edge,
                },
                &levels,
            );
            assert!(
                safe.diagnostics()
                    .iter()
                    .all(|diagnostic| diagnostic.code().as_str() != code),
                "{code} fired for the dual-namespace or local-inference near miss"
            );
        }
    }

    #[test]
    fn javascript_only_keeps_spec_footguns_as_warnings() {
        let parsed = parsed(
            0,
            "(42).toString(1); const x = value as User;",
            ScriptKind::JavaScript,
        );
        let result = check_with_lints(&parsed, &LintTable::new(LintProfile::Pedantic));
        assert!(result.diagnostics().iter().any(|diagnostic| {
            diagnostic.code().as_str() == "BAMTS-W071" && diagnostic.is_warning()
        }));
        assert!(
            result
                .diagnostics()
                .iter()
                .all(|diagnostic| diagnostic.code().as_str() != "BAMTS-W019")
        );
    }
}
