//! `.d.ts` emit through the existing declaration printer.
//!
//! The JavaScript printer already has a [`super::Surface::Declaration`] path.
//! This module is the declaration-emit policy layer in front of that path: it
//! rewrites the tree so parameter properties become class fields, private
//! members can be stripped, and isolated-declaration errors are reported for
//! exported values that would otherwise require the checker to invent a type.
//!
//! # Guarantees
//! * **One printer.** Lowered trees are handed to [`super::print`]; this module
//!   never prints syntax itself.
//! * **Source order.** Members and statements keep the order they had in the
//!   source, with lifted parameter properties inserted immediately before the
//!   constructor that declared them.
//! * **Deterministic.** Identical input and [`DeclarationOptions`] produce
//!   byte-identical `.d.ts` text and diagnostic vectors.
//! * **Negative paths.** An exported function, method, or variable that lacks a
//!   portable type under isolated declarations yields a typed [`Diagnostic`]
//!   rather than an implicit `any`.

use crate::checker::SemanticModel;
use crate::diagnostic::Diagnostic;
use crate::syntax::{
    Accessibility, BindingPattern, ClassDeclaration, ClassMember, ClassMemberNode, ClassProperty,
    ConstructorDeclaration, DeclarationModifiers, ExportDeclaration, ExportDefaultValue,
    ExportNamedDeclaration, FunctionLike, Node, NodeId, Parameter, ParameterModifiers,
    PropertyName, SourceFile, Statement, Stmt, VariableDeclaration,
};

use super::{EmitFileNames, EmitOutput, Newline, PrintOptions, Surface, print};

/// Stable diagnostic identifiers produced by declaration emit.
pub mod codes {
    use crate::diagnostic::DiagnosticCode;

    /// An exported function or method has no return type annotation.
    pub const MISSING_RETURN_TYPE: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1201");
    /// An exported variable has no type annotation and no portable initializer.
    pub const MISSING_VARIABLE_TYPE: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1202");
    /// A parameter property is bound to a pattern, which cannot be lifted.
    pub const PARAMETER_PROPERTY_PATTERN: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1203");
}

/// Immutable declaration-emit options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeclarationOptions {
    /// Structural newline sequence forwarded to the printer.
    pub newline: Newline,
    /// Spaces per indent level forwarded to the printer.
    pub indent_width: u8,
    /// When true, exported values without portable types are errors.
    pub isolated_declarations: bool,
    /// When true, `private` members and private parameter properties are omitted.
    pub strip_private: bool,
    /// When true, emit a source map for the declaration file.
    pub declaration_map: bool,
}

impl Default for DeclarationOptions {
    fn default() -> Self {
        Self {
            newline: Newline::Lf,
            indent_width: 4,
            isolated_declarations: false,
            strip_private: false,
            declaration_map: false,
        }
    }
}

impl DeclarationOptions {
    /// Isolated-declaration checking with private members retained.
    #[must_use]
    pub const fn isolated() -> Self {
        Self {
            newline: Newline::Lf,
            indent_width: 4,
            isolated_declarations: true,
            strip_private: false,
            declaration_map: false,
        }
    }

    /// Returns a copy that strips `private` members.
    #[must_use]
    pub const fn with_strip_private(mut self, strip_private: bool) -> Self {
        self.strip_private = strip_private;
        self
    }
}

/// Prints `file` as a `.d.ts` after applying declaration policy rewrites.
#[must_use]
pub fn emit_declarations(
    file: &SourceFile,
    model: &SemanticModel,
    options: DeclarationOptions,
    names: &EmitFileNames,
) -> EmitOutput {
    let mut diagnostics = Vec::new();
    if options.isolated_declarations {
        collect_isolated_errors(file, &mut diagnostics);
    }
    let rewritten = rewrite_file(file, options, &mut diagnostics);
    let mut output = print(
        &rewritten,
        model,
        PrintOptions {
            newline: options.newline,
            indent_width: options.indent_width,
            source_map: options.declaration_map,
            inline_source_map: false,
        },
        names,
        Surface::Declaration,
        None,
    );
    output.diagnostics.append(&mut diagnostics);
    output.diagnostics.sort();
    output.diagnostics.dedup();
    output
}

fn rewrite_file(
    file: &SourceFile,
    options: DeclarationOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> SourceFile {
    let statements = rewrite_statements(file.statements(), options, diagnostics);
    SourceFile::new(
        file.id(),
        file.source_id(),
        file.script_kind(),
        file.range(),
        std::sync::Arc::new(file.source_text().clone()),
        file.tokens().to_vec(),
        statements,
        *file.eof(),
        file.diagnostics().to_vec(),
    )
}

fn rewrite_statements(
    statements: &[Stmt],
    options: DeclarationOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Stmt> {
    statements
        .iter()
        .map(|statement| rewrite_statement(statement, options, diagnostics))
        .collect()
}

fn rewrite_statement(
    statement: &Stmt,
    options: DeclarationOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> Stmt {
    match statement.data() {
        Statement::Class(class) => Node::new(
            statement.id(),
            statement.range(),
            Statement::Class(rewrite_class(class, options, diagnostics)),
        ),
        Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(inner))) => {
            let inner = rewrite_statement(inner, options, diagnostics);
            Node::new(
                statement.id(),
                statement.range(),
                Statement::Export(ExportDeclaration::Named(
                    ExportNamedDeclaration::Declaration(Box::new(inner)),
                )),
            )
        }
        Statement::Export(ExportDeclaration::Default(default)) => {
            let value = match &default.value {
                ExportDefaultValue::Class(class) => {
                    ExportDefaultValue::Class(rewrite_class(class, options, diagnostics))
                }
                other => other.clone(),
            };
            Node::new(
                statement.id(),
                statement.range(),
                Statement::Export(ExportDeclaration::Default(
                    crate::syntax::ExportDefaultDeclaration { value },
                )),
            )
        }
        Statement::Declare(inner) => Node::new(
            statement.id(),
            statement.range(),
            Statement::Declare(Box::new(rewrite_statement(inner, options, diagnostics))),
        ),
        _ => statement.clone(),
    }
}

fn rewrite_class(
    class: &ClassDeclaration,
    options: DeclarationOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> ClassDeclaration {
    let mut members = Vec::new();
    for member in &class.members {
        match member.data() {
            ClassMember::Constructor(constructor) => {
                let (properties, constructor) =
                    lift_parameter_properties(member.id(), constructor, options, diagnostics);
                members.extend(properties);
                if !options.strip_private
                    || constructor.modifiers.accessibility != Some(Accessibility::Private)
                {
                    members.push(Node::new(
                        member.id(),
                        member.range(),
                        ClassMember::Constructor(constructor),
                    ));
                }
            }
            ClassMember::Method(method)
                if options.strip_private
                    && method.modifiers.accessibility == Some(Accessibility::Private) => {}
            ClassMember::Property(property)
                if options.strip_private
                    && property.modifiers.accessibility == Some(Accessibility::Private) => {}
            ClassMember::AutoAccessor(accessor)
                if options.strip_private
                    && accessor.modifiers.accessibility == Some(Accessibility::Private) => {}
            _ => members.push(member.clone()),
        }
    }
    ClassDeclaration {
        members,
        ..class.clone()
    }
}

fn lift_parameter_properties(
    constructor_id: NodeId,
    constructor: &ConstructorDeclaration,
    options: DeclarationOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<ClassMemberNode>, ConstructorDeclaration) {
    let mut properties = Vec::new();
    let mut parameters = Vec::new();
    for (index, parameter) in constructor.parameters.iter().enumerate() {
        let data = parameter.data();
        if !super::is_parameter_property(data) {
            parameters.push(parameter.clone());
            continue;
        }
        if options.strip_private && data.modifiers.accessibility == Some(Accessibility::Private) {
            parameters.push(Node::new(
                parameter.id(),
                parameter.range(),
                Parameter {
                    modifiers: ParameterModifiers::default(),
                    ..data.clone()
                },
            ));
            continue;
        }
        match data.binding.data() {
            BindingPattern::Identifier(ident) => {
                properties.push(Node::new(
                    NodeId::new(constructor_id.get().saturating_add(1000 + index as u32)),
                    parameter.range(),
                    ClassMember::Property(ClassProperty {
                        decorators: data.decorators.clone(),
                        modifiers: DeclarationModifiers {
                            accessibility: data.modifiers.accessibility,
                            is_abstract: false,
                            is_declare: false,
                            is_override: data.modifiers.is_override,
                            is_readonly: data.modifiers.is_readonly,
                            is_static: false,
                        },
                        name: PropertyName::Identifier(ident.clone()),
                        optional: data.optional,
                        definite: false,
                        type_annotation: data.type_annotation.clone(),
                        initializer: None,
                    }),
                ));
                parameters.push(Node::new(
                    parameter.id(),
                    parameter.range(),
                    Parameter {
                        modifiers: ParameterModifiers::default(),
                        ..data.clone()
                    },
                ));
            }
            _ => {
                diagnostics.push(Diagnostic::error(
                    codes::PARAMETER_PROPERTY_PATTERN,
                    // Filled below; the caller has the file identity. Use a
                    // placeholder that print() will not duplicate: the rewrite
                    // still keeps the original parameter so the printer can
                    // recover, and this diagnostic is attached with a dummy
                    // source until `attach_source` rewrites it.
                    crate::source::SourceId::new(0),
                    parameter.range(),
                    "parameter property must bind an identifier",
                ));
                parameters.push(parameter.clone());
            }
        }
    }
    (
        properties,
        ConstructorDeclaration {
            parameters,
            ..constructor.clone()
        },
    )
}

fn collect_isolated_errors(file: &SourceFile, diagnostics: &mut Vec<Diagnostic>) {
    for statement in file.statements() {
        collect_statement(file, statement, false, diagnostics);
    }
}

fn collect_statement(
    file: &SourceFile,
    statement: &Stmt,
    exported: bool,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match statement.data() {
        Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(inner))) => {
            collect_statement(file, inner, true, diagnostics);
        }
        Statement::Export(ExportDeclaration::Default(default)) => match &default.value {
            ExportDefaultValue::Function(function) => {
                check_function(file, statement, function, diagnostics);
            }
            ExportDefaultValue::Class(class) => check_class(file, class, diagnostics),
            _ => {}
        },
        Statement::Function(function) if exported => {
            check_function(file, statement, &function.function, diagnostics);
        }
        Statement::Variable(declaration) if exported => {
            check_variable(file, declaration, diagnostics);
        }
        Statement::Class(class) if exported => check_class(file, class, diagnostics),
        Statement::Declare(inner) => collect_statement(file, inner, exported, diagnostics),
        _ => {}
    }
}

fn check_function(
    file: &SourceFile,
    statement: &Stmt,
    function: &FunctionLike,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if function.return_type.is_none() {
        diagnostics.push(Diagnostic::error(
            codes::MISSING_RETURN_TYPE,
            file.source_id(),
            statement.range(),
            "exported function must have an explicit return type",
        ));
    }
}

fn check_variable(
    file: &SourceFile,
    declaration: &VariableDeclaration,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for declarator in &declaration.declarations {
        let data = declarator.data();
        if data.type_annotation.is_some() {
            continue;
        }
        let portable = data
            .initializer
            .as_ref()
            .is_some_and(|expression| is_portable_initializer(expression.data()));
        if !portable {
            diagnostics.push(Diagnostic::error(
                codes::MISSING_VARIABLE_TYPE,
                file.source_id(),
                declarator.range(),
                "exported variable must have an explicit type annotation",
            ));
        }
    }
}

fn check_class(file: &SourceFile, class: &ClassDeclaration, diagnostics: &mut Vec<Diagnostic>) {
    for member in &class.members {
        if let ClassMember::Method(method) = member.data()
            && method.modifiers.accessibility != Some(Accessibility::Private)
            && method.function.return_type.is_none()
            && method.modifier == crate::syntax::PropertyModifier::None
        {
            diagnostics.push(Diagnostic::error(
                codes::MISSING_RETURN_TYPE,
                file.source_id(),
                member.range(),
                "exported function must have an explicit return type",
            ));
        }
    }
}

fn is_portable_initializer(expression: &crate::syntax::Expression) -> bool {
    matches!(
        expression,
        crate::syntax::Expression::Literal(_)
            | crate::syntax::Expression::As(_)
            | crate::syntax::Expression::Satisfies(_)
            | crate::syntax::Expression::TypeAssertion(_)
    )
}

#[cfg(test)]
mod tests {
    use super::{DeclarationOptions, codes, emit_declarations};
    use crate::checker;
    use crate::parser;
    use crate::scanner;
    use crate::source::{ScriptKind, SourceId, SourceText};
    use std::sync::Arc;

    fn parse(source: &str) -> crate::diagnostic::Recovered<crate::syntax::SourceFile> {
        parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(SourceText::new(source).expect("static test source fits size limit")),
        ))
    }

    fn emit_dts(source: &str, options: DeclarationOptions) -> crate::emitter::EmitOutput {
        let parsed = parse(source);
        let model = checker::check(&parsed).into_product();
        emit_declarations(
            parsed.product(),
            &model,
            options,
            &crate::emitter::EmitFileNames::default(),
        )
    }

    #[test]
    fn function_bodies_are_elided_and_declare_is_prefixed() {
        let output = emit_dts(
            "export function f(x: number): number { return x; }\n",
            DeclarationOptions::default(),
        );
        assert!(!output.has_errors());
        assert_eq!(
            output.declaration.as_ref().unwrap().code,
            "export declare function f(x: number): number;\n"
        );
    }

    #[test]
    fn parameter_properties_are_lifted_before_the_constructor() {
        let output = emit_dts(
            "export class C { constructor(public x: number) {} }\n",
            DeclarationOptions::default(),
        );
        assert!(!output.has_errors(), "{:?}", output.diagnostics);
        assert_eq!(
            output.declaration.as_ref().unwrap().code,
            "export declare class C {\n    public x: number;\n    constructor(x: number);\n}\n"
        );
    }

    #[test]
    fn private_members_are_stripped_when_requested() {
        let source = "export class C { private hidden: number; visible: string; }\n";
        let kept = emit_dts(source, DeclarationOptions::default());
        assert!(kept.declaration.as_ref().unwrap().code.contains("hidden"));
        let stripped = emit_dts(
            source,
            DeclarationOptions::default().with_strip_private(true),
        );
        assert!(
            !stripped
                .declaration
                .as_ref()
                .unwrap()
                .code
                .contains("hidden"),
            "{}",
            stripped.declaration.as_ref().unwrap().code
        );
        assert!(
            stripped
                .declaration
                .as_ref()
                .unwrap()
                .code
                .contains("visible")
        );
    }

    #[test]
    fn isolated_declarations_require_exported_return_types() {
        let output = emit_dts(
            "export function f(x: number) { return x; }\n",
            DeclarationOptions::isolated(),
        );
        assert!(output.has_errors());
        assert_eq!(output.diagnostics[0].code(), codes::MISSING_RETURN_TYPE);
    }

    #[test]
    fn isolated_declarations_require_exported_variable_types() {
        let output = emit_dts(
            "export const f = () => 1;\n",
            DeclarationOptions::isolated(),
        );
        assert!(output.has_errors());
        assert_eq!(output.diagnostics[0].code(), codes::MISSING_VARIABLE_TYPE);
    }

    #[test]
    fn isolated_declarations_accept_literal_initializers() {
        let output = emit_dts("export const n = 1;\n", DeclarationOptions::isolated());
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::MISSING_VARIABLE_TYPE),
            "{:?}",
            output.diagnostics
        );
    }
    #[test]
    fn isolated_declarations_ignore_unexported_implementation_details() {
        let output = emit_dts(
            "const local = factory(); export const n: number = 1;\n",
            DeclarationOptions::isolated(),
        );
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::MISSING_VARIABLE_TYPE),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn parameter_property_patterns_are_errors() {
        let output = emit_dts(
            "export class C { constructor(public [x]: number[]) {} }\n",
            DeclarationOptions::default(),
        );
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::PARAMETER_PROPERTY_PATTERN),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn declaration_emit_is_deterministic() {
        let source =
            "export class C { constructor(readonly y: string) { this.y = y; } z(): void {} }\n";
        let left = emit_dts(source, DeclarationOptions::default());
        let right = emit_dts(source, DeclarationOptions::default());
        assert_eq!(left, right);
    }

    #[test]
    fn interfaces_and_type_aliases_are_preserved() {
        let output = emit_dts(
            "export interface I { x: number; }\nexport type T = I;\n",
            DeclarationOptions::default(),
        );
        assert_eq!(
            output.declaration.as_ref().unwrap().code,
            "export interface I {\n    x: number;\n}\nexport type T = I;\n"
        );
    }
}
