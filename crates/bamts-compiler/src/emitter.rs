//! Deterministic AST-to-JavaScript and declaration emit.
//!
//! The checked [`emit_checked`] path walks an immutable [`SourceFile`] and
//! [`SemanticModel`] and prints either runtime JavaScript ([`EmitMode::JavaScript`])
//! or a TypeScript declaration file ([`EmitMode::Declaration`]). The compatibility
//! [`emit`] entry point performs that semantic pass before delegating.
//!
//! # Guarantees
//! * **Deterministic.** The same tree and [`EmitOptions`] always produce byte-
//!   identical output. Structural line breaks follow [`Newline`]; newlines that
//!   live inside a literal (template/string/regex lexeme) are copied verbatim so
//!   the value is never altered.
//! * **Type erasure.** In JavaScript mode all type-only syntax is removed:
//!   annotations, type parameters/arguments, `interface`/`type`, `declare`,
//!   `import type`, `as`/`satisfies`/`!`/`<T>` assertions, `implements`, and
//!   parameter `?`/`!` markers. Parameter properties are lowered to constructor
//!   assignments and `enum` is lowered to its canonical runtime object.
//! * **Correct precedence.** Parentheses are re-derived from operator
//!   precedence and associativity rather than copied from the source, so the
//!   printed grouping always matches the tree.
//! * **Stable recovery diagnostics.** Nodes the parser could only recover as a
//!   [`MissingNode`], and constructs that cannot be lowered without the checker
//!   (`namespace` runtime lowering), yield ordered typed [`Diagnostic`] values
//!   while a best-effort product is still returned.
//!
//! Constant folding, type inference for un-annotated declarations, and lexical
//! reference rewriting are the checker's responsibility and are intentionally
//! out of scope here.

use std::{fmt::Write as _, sync::Arc};

use crate::checker::{self, SemanticModel};
use crate::diagnostic::{Diagnostic, DiagnosticCode, Recovered};
use crate::enum_plan::{EnumFacts, EnumMemberPlan, EnumScalar};
use crate::source::{SourceId, SourceText, TextRange};
use crate::syntax::*;

/// Stable diagnostic identifiers produced by the emitter.
pub mod codes {
    use crate::diagnostic::DiagnosticCode;

    /// A recovered expression node has no printable form.
    pub const MISSING_EXPRESSION: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1001");
    /// A recovered statement node has no printable form.
    pub const MISSING_STATEMENT: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1002");
    /// A recovered type node has no printable form.
    pub const MISSING_TYPE: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1003");
    /// A recovered binding pattern has no printable form.
    pub const MISSING_BINDING: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1004");
    /// A recovered assignment target has no printable form.
    pub const MISSING_TARGET: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1005");
    /// A recovered property name has no printable form.
    pub const MISSING_PROPERTY_NAME: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1006");
    /// A recovered module or entity name has no printable form.
    pub const MISSING_NAME: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1007");
    /// A recovered member (class/object/type) node has no printable form.
    pub const MISSING_MEMBER: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1008");
    /// A recovered element (array/argument) node has no printable form.
    pub const MISSING_ELEMENT: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1009");
    /// A token range is not a valid slice of the source text.
    pub const UNRESOLVED_TOKEN: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1010");
    /// `namespace` runtime lowering needs semantic analysis unavailable here.
    pub const NAMESPACE_UNLOWERED: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1011");
    /// A runtime enum declaration has no matching checked enum plan.
    pub const ENUM_FACTS_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1013");
}

/// Which surface the emitter prints.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EmitMode {
    /// Runtime JavaScript with all type-only syntax erased.
    JavaScript,
    /// A TypeScript declaration (`.d.ts`) file.
    Declaration,
}

/// The structural line terminator the emitter inserts between lines.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Newline {
    /// Unix line feed (`\n`).
    Lf,
    /// Windows carriage return + line feed (`\r\n`).
    CrLf,
}

impl Newline {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// Immutable printing options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EmitOptions {
    /// The surface to print.
    pub mode: EmitMode,
    /// The structural newline sequence.
    pub newline: Newline,
    /// The number of spaces per indentation level.
    pub indent_width: u8,
}

impl Default for EmitOptions {
    fn default() -> Self {
        Self {
            mode: EmitMode::JavaScript,
            newline: Newline::Lf,
            indent_width: 4,
        }
    }
}

impl EmitOptions {
    /// Default JavaScript options (LF newlines, four-space indentation).
    #[must_use]
    pub const fn javascript() -> Self {
        Self {
            mode: EmitMode::JavaScript,
            newline: Newline::Lf,
            indent_width: 4,
        }
    }

    /// Default declaration options (LF newlines, four-space indentation).
    #[must_use]
    pub const fn declaration() -> Self {
        Self {
            mode: EmitMode::Declaration,
            newline: Newline::Lf,
            indent_width: 4,
        }
    }

    /// Returns a copy using `newline` for structural line breaks.
    #[must_use]
    pub const fn with_newline(mut self, newline: Newline) -> Self {
        self.newline = newline;
        self
    }

    /// Returns a copy using `indent_width` spaces per level.
    #[must_use]
    pub const fn with_indent_width(mut self, indent_width: u8) -> Self {
        self.indent_width = indent_width;
        self
    }
}

/// The recovered emitter product: printed text plus ordered diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmitOutput {
    /// The printed program text.
    pub code: String,
    /// Diagnostics in canonical [`Diagnostic`] order.
    pub diagnostics: Vec<Diagnostic>,
}

impl EmitOutput {
    /// Returns whether any diagnostic is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| !d.is_warning())
    }
}

/// Prints `file` to JavaScript or a declaration file per `options`.
#[must_use]
pub fn emit(file: &SourceFile, options: EmitOptions) -> EmitOutput {
    let checked_file = Recovered::clean(SourceFile::new(
        file.id(),
        file.source_id(),
        file.script_kind(),
        file.range(),
        Arc::new(file.source_text().clone()),
        file.tokens().to_vec(),
        file.statements().to_vec(),
        *file.eof(),
        file.diagnostics().to_vec(),
    ));
    let checked = checker::check(&checked_file);
    emit_checked(file, checked.product(), options)
}

/// Prints `file` using the semantic model produced by its checker pass.
#[must_use]
pub fn emit_checked(file: &SourceFile, model: &SemanticModel, options: EmitOptions) -> EmitOutput {
    let mut emitter = Emitter {
        source: file.source_text(),
        source_id: file.source_id(),
        model,
        enum_facts: model.enum_facts(),
        options,
        out: String::new(),
        indent: 0,
        pending_indent: false,
        anchor: file.range(),
        diagnostics: Vec::new(),
        decl_ambient: false,
        decl_in_export: false,
    };
    match options.mode {
        EmitMode::JavaScript => emitter.emit_module_js(file.statements()),
        EmitMode::Declaration => emitter.emit_module_decl(file.statements()),
    }
    emitter.diagnostics.sort();
    EmitOutput {
        code: emitter.out,
        diagnostics: emitter.diagnostics,
    }
}

// Operator precedence levels: larger binds tighter.
const P_SEQUENCE: u8 = 1;
const P_ASSIGN: u8 = 2;
const P_CONDITIONAL: u8 = 3;
const P_NULLISH: u8 = 4;
const P_LOGICAL_OR: u8 = 4;
const P_LOGICAL_AND: u8 = 5;
const P_BIT_OR: u8 = 6;
const P_BIT_XOR: u8 = 7;
const P_BIT_AND: u8 = 8;
const P_EQUALITY: u8 = 9;
const P_RELATIONAL: u8 = 10;
const P_SHIFT: u8 = 11;
const P_ADDITIVE: u8 = 12;
const P_MULTIPLICATIVE: u8 = 13;
const P_EXPONENT: u8 = 14;
const P_UNARY: u8 = 15;
const P_POSTFIX: u8 = 16;
const P_CALL_MEMBER: u8 = 17;
const P_PRIMARY: u8 = 18;

struct Emitter<'a> {
    source: &'a SourceText,
    source_id: SourceId,
    model: &'a SemanticModel,
    enum_facts: &'a EnumFacts,
    options: EmitOptions,
    out: String,
    indent: usize,
    pending_indent: bool,
    anchor: TextRange,
    diagnostics: Vec<Diagnostic>,
    decl_ambient: bool,
    decl_in_export: bool,
}

impl<'a> Emitter<'a> {
    // ---- low level output -------------------------------------------------

    fn raw(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.pending_indent {
            let spaces = self.indent * self.options.indent_width as usize;
            for _ in 0..spaces {
                self.out.push(' ');
            }
            self.pending_indent = false;
        }
        self.out.push_str(text);
    }

    fn newline(&mut self) {
        self.out.push_str(self.options.newline.as_str());
        self.pending_indent = true;
    }

    fn diag(&mut self, code: DiagnosticCode, message: &'static str, range: TextRange) {
        self.diagnostics
            .push(Diagnostic::error(code, self.source_id, range, message));
    }

    fn diag_here(&mut self, code: DiagnosticCode, message: &'static str) {
        let range = self.anchor;
        self.diag(code, message, range);
    }

    /// Zero-copy lexeme for a token, or `None` for an invalid slice.
    fn text(&self, token: &Token) -> Option<&'a str> {
        if token.is_missing() {
            return Some("");
        }
        let source: &'a SourceText = self.source;
        let range = token.range();
        let start = source.utf16_to_byte(range.start()).ok()?;
        let end = source.utf16_to_byte(range.end()).ok()?;
        source.as_str().get(start..end)
    }

    fn emit_token(&mut self, token: &Token) {
        match self.text(token) {
            Some(text) => self.raw(text),
            None => {
                let range = token.range();
                self.diag(
                    codes::UNRESOLVED_TOKEN,
                    "token text is not a valid slice of the source",
                    range,
                );
            }
        }
    }

    fn emit_ident(&mut self, ident: &IdentifierNode) {
        self.emit_token(ident.data().token());
    }

    fn emit_string(&mut self, literal: &StringLiteralNode) {
        self.emit_token(literal.data().token());
    }

    // ---- module drivers ---------------------------------------------------

    fn emit_module_js(&mut self, statements: &[Stmt]) {
        for statement in statements {
            if self.emit_statement(statement) {
                self.newline();
            }
        }
    }

    fn emit_module_decl(&mut self, statements: &[Stmt]) {
        self.decl_ambient = true;
        for statement in statements {
            if self.emit_declaration(statement) {
                self.newline();
            }
        }
    }

    // =======================================================================
    // JavaScript statements
    // =======================================================================

    fn emit_statement(&mut self, statement: &Stmt) -> bool {
        let previous = self.anchor;
        self.anchor = statement.range();
        let emitted = match statement.data() {
            Statement::Import(import) => self.emit_import(import, false),
            Statement::ImportEquals(import) => {
                if import.is_type_only {
                    false
                } else {
                    self.emit_import_equals_js(import);
                    true
                }
            }
            Statement::Export(export) => self.emit_export_js(export),
            Statement::Variable(declaration) => {
                self.emit_variable_head(declaration);
                self.raw(";");
                true
            }
            Statement::Function(function) => {
                if function.function.body.is_none() {
                    false
                } else {
                    self.emit_function_declaration_js(&function.function);
                    true
                }
            }
            Statement::Class(class) => {
                self.emit_class_js(class);
                true
            }
            Statement::Interface(_) | Statement::TypeAlias(_) | Statement::Declare(_) => false,
            Statement::Enum(declaration) => self.emit_enum_js(statement.id(), declaration),
            Statement::Namespace(_) => {
                self.diag_here(
                    codes::NAMESPACE_UNLOWERED,
                    "namespace runtime lowering requires semantic analysis",
                );
                false
            }
            Statement::Block(block) => {
                self.emit_block(block.data());
                true
            }
            Statement::Empty => false,
            Statement::Expression(statement) => {
                self.emit_expression_statement(statement);
                true
            }
            Statement::If(_) => {
                self.emit_if(statement);
                true
            }
            Statement::Switch(switch) => {
                self.emit_switch(switch);
                true
            }
            Statement::For(statement) => {
                self.emit_for(statement);
                true
            }
            Statement::ForIn(statement) => {
                self.emit_for_in(statement);
                true
            }
            Statement::ForOf(statement) => {
                self.emit_for_of(statement);
                true
            }
            Statement::While(statement) => {
                self.raw("while (");
                self.emit_expression(&statement.test);
                self.raw(")");
                self.emit_control_body(&statement.body);
                true
            }
            Statement::DoWhile(statement) => {
                self.raw("do");
                self.emit_control_body(&statement.body);
                self.raw(" while (");
                self.emit_expression(&statement.test);
                self.raw(");");
                true
            }
            Statement::Try(statement) => {
                self.emit_try(statement);
                true
            }
            Statement::With(statement) => {
                self.raw("with (");
                self.emit_expression(&statement.object);
                self.raw(")");
                self.emit_control_body(&statement.body);
                true
            }
            Statement::Labeled(statement) => {
                self.emit_ident(&statement.label);
                self.raw(": ");
                self.emit_statement(&statement.body);
                true
            }
            Statement::Break(jump) => {
                self.raw("break");
                if let Some(label) = &jump.label {
                    self.raw(" ");
                    self.emit_ident(label);
                }
                self.raw(";");
                true
            }
            Statement::Continue(jump) => {
                self.raw("continue");
                if let Some(label) = &jump.label {
                    self.raw(" ");
                    self.emit_ident(label);
                }
                self.raw(";");
                true
            }
            Statement::Return(statement) => {
                self.raw("return");
                if let Some(argument) = &statement.argument {
                    self.raw(" ");
                    self.emit_expression(argument);
                }
                self.raw(";");
                true
            }
            Statement::Throw(statement) => {
                self.raw("throw ");
                self.emit_expression(&statement.argument);
                self.raw(";");
                true
            }
            Statement::Debugger => {
                self.raw("debugger;");
                true
            }
            Statement::Missing(_) => {
                self.diag_here(
                    codes::MISSING_STATEMENT,
                    "cannot emit a missing statement node",
                );
                self.raw(";");
                true
            }
        };
        self.anchor = previous;
        emitted
    }

    fn emit_expression_statement(&mut self, statement: &ExpressionStatement) {
        let expression = &statement.expression;
        let wrap = self.leads_with_bad_token(expression, false);
        if wrap {
            self.raw("(");
            self.emit_expression_prec(expression, 0);
            self.raw(")");
        } else {
            self.emit_expression_prec(expression, 0);
        }
        self.raw(";");
    }

    fn emit_if(&mut self, statement: &Stmt) {
        let Statement::If(if_statement) = statement.data() else {
            return;
        };
        self.raw("if (");
        self.emit_expression(&if_statement.test);
        self.raw(")");
        self.emit_control_body(&if_statement.consequent);
        if let Some(alternate) = &if_statement.alternate {
            if matches!(alternate.data(), Statement::If(_)) {
                self.raw(" else ");
                self.emit_if(alternate);
            } else {
                self.raw(" else");
                self.emit_control_body(alternate);
            }
        }
    }

    fn emit_control_body(&mut self, statement: &Stmt) {
        self.raw(" ");
        match statement.data() {
            Statement::Block(block) => self.emit_block(block.data()),
            Statement::Empty => self.raw("{}"),
            _ => {
                self.raw("{");
                self.newline();
                self.indent += 1;
                if self.emit_statement(statement) {
                    self.newline();
                }
                self.indent -= 1;
                self.raw("}");
            }
        }
    }

    fn emit_block(&mut self, block: &Block) {
        if block.statements.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        for statement in &block.statements {
            if self.emit_statement(statement) {
                self.newline();
            }
        }
        self.indent -= 1;
        self.raw("}");
    }

    fn emit_switch(&mut self, switch: &SwitchStatement) {
        self.raw("switch (");
        self.emit_expression(&switch.discriminant);
        self.raw(") ");
        if switch.cases.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        for case in &switch.cases {
            let case = case.data();
            match &case.test {
                Some(test) => {
                    self.raw("case ");
                    self.emit_expression(test);
                    self.raw(":");
                }
                None => self.raw("default:"),
            }
            self.newline();
            if !case.consequent.is_empty() {
                self.indent += 1;
                for statement in &case.consequent {
                    if self.emit_statement(statement) {
                        self.newline();
                    }
                }
                self.indent -= 1;
            }
        }
        self.indent -= 1;
        self.raw("}");
    }

    fn emit_for(&mut self, statement: &ForStatement) {
        self.raw("for (");
        match &statement.initializer {
            Some(ForInitializer::Variable(declaration)) => self.emit_variable_head(declaration),
            Some(ForInitializer::Expression(expression)) => {
                self.emit_expression_prec(expression, 0);
            }
            None => {}
        }
        self.raw(";");
        if let Some(test) = &statement.test {
            self.raw(" ");
            self.emit_expression(test);
        }
        self.raw(";");
        if let Some(update) = &statement.update {
            self.raw(" ");
            self.emit_expression(update);
        }
        self.raw(")");
        self.emit_control_body(&statement.body);
    }

    fn emit_for_in(&mut self, statement: &ForInStatement) {
        self.raw("for (");
        self.emit_for_binding(&statement.binding);
        self.raw(" in ");
        self.emit_expression_prec(&statement.object, P_ASSIGN);
        self.raw(")");
        self.emit_control_body(&statement.body);
    }

    fn emit_for_of(&mut self, statement: &ForOfStatement) {
        if matches!(statement.mode, ForOfMode::Async) {
            self.raw("for await (");
        } else {
            self.raw("for (");
        }
        self.emit_for_binding(&statement.binding);
        self.raw(" of ");
        self.emit_expression_prec(&statement.iterable, P_ASSIGN);
        self.raw(")");
        self.emit_control_body(&statement.body);
    }

    fn emit_for_binding(&mut self, binding: &ForBinding) {
        match binding {
            ForBinding::Variable(declaration) => self.emit_variable_head(declaration),
            ForBinding::Target(target) => self.emit_assignment_target(target),
        }
    }

    fn emit_try(&mut self, statement: &TryStatement) {
        self.raw("try ");
        self.emit_block(statement.block.data());
        if let Some(handler) = &statement.handler {
            let handler = handler.data();
            self.raw(" catch");
            if let Some(binding) = &handler.binding {
                self.raw(" (");
                self.emit_pattern(binding);
                self.raw(")");
            }
            self.raw(" ");
            self.emit_block(handler.body.data());
        }
        if let Some(finalizer) = &statement.finalizer {
            self.raw(" finally ");
            self.emit_block(finalizer.data());
        }
    }

    fn emit_variable_head(&mut self, declaration: &VariableDeclaration) {
        self.raw(variable_kind_str(declaration.kind));
        self.raw(" ");
        for (index, declarator) in declaration.declarations.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            let declarator = declarator.data();
            self.emit_pattern(&declarator.binding);
            if let Some(initializer) = &declarator.initializer {
                self.raw(" = ");
                self.emit_expression_prec(initializer, P_ASSIGN);
            }
        }
    }

    // ---- imports / exports (JavaScript) -----------------------------------

    fn emit_import(&mut self, import: &ImportDeclaration, keep_types: bool) -> bool {
        if import.type_only && !keep_types {
            return false;
        }

        // A clause that resolves to no value bindings is fully type-only and is
        // erased in JavaScript (a bare side-effect import keeps its clause-less
        // form below).
        if !keep_types && let Some(clause) = &import.clause {
            let has_value = clause.default.is_some()
                || matches!(&clause.binding, Some(ImportBinding::Namespace(_)))
                || matches!(
                    &clause.binding,
                    Some(ImportBinding::Named(specifiers))
                        if specifiers
                            .iter()
                            .any(|s| matches!(s.data().mode, ImportSpecifierMode::Value))
                );
            if !has_value {
                return false;
            }
        }

        self.raw("import ");
        if keep_types && import.type_only {
            self.raw("type ");
        }
        if let Some(clause) = &import.clause {
            let mut wrote = false;
            if let Some(default) = &clause.default {
                self.emit_ident(default);
                wrote = true;
            }
            if let Some(binding) = &clause.binding {
                match binding {
                    ImportBinding::Namespace(name) => {
                        if wrote {
                            self.raw(", ");
                        }
                        self.raw("* as ");
                        self.emit_ident(name);
                    }
                    ImportBinding::Named(specifiers) => {
                        if wrote {
                            self.raw(", ");
                        }
                        self.emit_named_imports(specifiers, keep_types);
                    }
                }
            }
            self.raw(" from ");
        }
        self.emit_string(&import.source);
        if let Some(attributes) = &import.attributes {
            self.emit_import_attributes(attributes);
        }
        self.raw(";");
        true
    }

    fn emit_named_imports(&mut self, specifiers: &[ImportSpecifierNode], keep_types: bool) {
        let visible: Vec<&ImportSpecifierNode> = specifiers
            .iter()
            .filter(|s| keep_types || matches!(s.data().mode, ImportSpecifierMode::Value))
            .collect();
        if visible.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{ ");
        for (index, specifier) in visible.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            let specifier = specifier.data();
            if keep_types && matches!(specifier.mode, ImportSpecifierMode::TypeOnly) {
                self.raw("type ");
            }
            if self.module_name_matches_ident(&specifier.imported, &specifier.local) {
                self.emit_ident(&specifier.local);
            } else {
                self.emit_module_export_name(&specifier.imported);
                self.raw(" as ");
                self.emit_ident(&specifier.local);
            }
        }
        self.raw(" }");
    }

    fn emit_import_attributes(&mut self, attributes: &ImportAttributes) {
        if attributes.entries.is_empty() {
            return;
        }
        self.raw(" with { ");
        for (index, entry) in attributes.entries.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            self.emit_module_export_name(&entry.name);
            self.raw(": ");
            self.emit_string(&entry.value);
        }
        self.raw(" }");
    }

    fn emit_import_equals_js(&mut self, import: &ImportEqualsDeclaration) {
        self.raw("const ");
        self.emit_ident(&import.local);
        self.raw(" = ");
        match &import.reference {
            ExternalModuleReference::Require(source) => {
                self.raw("require(");
                self.emit_string(source);
                self.raw(")");
            }
            ExternalModuleReference::Qualified(name) => self.emit_entity_name(name),
            ExternalModuleReference::Missing(_) => {
                self.diag_here(
                    codes::MISSING_NAME,
                    "cannot emit a missing module reference",
                );
            }
        }
        self.raw(";");
    }

    fn emit_export_js(&mut self, export: &ExportDeclaration) -> bool {
        match export {
            ExportDeclaration::Named(ExportNamedDeclaration::Declaration(inner)) => {
                if !self.js_statement_emits(inner) {
                    // Trigger any recovery diagnostics without a dangling `export`.
                    self.emit_statement(inner);
                    return false;
                }
                if let Statement::Class(class) = inner.data() {
                    self.emit_decorators_block(&class.decorators);
                    self.raw("export ");
                    self.emit_class_core_js(class);
                    return true;
                }
                self.raw("export ");
                self.emit_statement(inner)
            }
            ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                type_only,
                specifiers,
                source,
                attributes,
            }) => {
                if *type_only {
                    return false;
                }
                let visible: Vec<&ExportSpecifierNode> = specifiers
                    .iter()
                    .filter(|s| matches!(s.data().mode, ExportSpecifierMode::Value))
                    .collect();
                if visible.is_empty() {
                    return false;
                }
                self.raw("export { ");
                for (index, specifier) in visible.iter().enumerate() {
                    if index > 0 {
                        self.raw(", ");
                    }
                    self.emit_export_specifier(specifier.data(), false);
                }
                self.raw(" }");
                if let Some(source) = source {
                    self.raw(" from ");
                    self.emit_string(source);
                    if let Some(attributes) = attributes {
                        self.emit_import_attributes(attributes);
                    }
                }
                self.raw(";");
                true
            }
            ExportDeclaration::All(all) => {
                if all.type_only {
                    return false;
                }
                self.raw("export * ");
                if let Some(name) = &all.exported {
                    self.raw("as ");
                    self.emit_module_export_name(name);
                    self.raw(" ");
                }
                self.raw("from ");
                self.emit_string(&all.source);
                if let Some(attributes) = &all.attributes {
                    self.emit_import_attributes(attributes);
                }
                self.raw(";");
                true
            }
            ExportDeclaration::Default(default) => match &default.value {
                ExportDefaultValue::Function(function) => {
                    self.raw("export default ");
                    self.emit_function_declaration_js(function);
                    true
                }
                ExportDefaultValue::Class(class) => {
                    self.emit_decorators_block(&class.decorators);
                    self.raw("export default ");
                    self.emit_class_core_js(class);
                    true
                }
                ExportDefaultValue::Expression(expression) => {
                    self.raw("export default ");
                    self.emit_expression_prec(expression, P_ASSIGN);
                    self.raw(";");
                    true
                }
                ExportDefaultValue::Missing(_) => {
                    self.diag_here(
                        codes::MISSING_EXPRESSION,
                        "cannot emit a missing default export",
                    );
                    false
                }
                ExportDefaultValue::Interface(_) => false,
            },
            ExportDeclaration::Assignment(expression) => {
                self.raw("export default ");
                self.emit_expression_prec(expression, P_ASSIGN);
                self.raw(";");
                true
            }
        }
    }

    fn emit_export_specifier(&mut self, specifier: &ExportSpecifier, keep_type: bool) {
        if keep_type && matches!(specifier.mode, ExportSpecifierMode::TypeOnly) {
            self.raw("type ");
        }
        if self.module_names_match(&specifier.local, &specifier.exported) {
            self.emit_module_export_name(&specifier.local);
        } else {
            self.emit_module_export_name(&specifier.local);
            self.raw(" as ");
            self.emit_module_export_name(&specifier.exported);
        }
    }

    fn js_statement_emits(&self, statement: &Stmt) -> bool {
        match statement.data() {
            Statement::Interface(_)
            | Statement::TypeAlias(_)
            | Statement::Declare(_)
            | Statement::Namespace(_)
            | Statement::Empty => false,
            Statement::Function(function) => function.function.body.is_some(),
            Statement::Import(import) => !import.type_only,
            Statement::Enum(declaration) => !declaration.is_const,
            _ => true,
        }
    }

    // =======================================================================
    // enum lowering (JavaScript)
    // =======================================================================

    fn emit_enum_js(&mut self, declaration_id: NodeId, declaration: &EnumDeclaration) -> bool {
        let Some(name) = self
            .text(declaration.name.data().token())
            .map(str::to_owned)
        else {
            self.diag(
                codes::UNRESOLVED_TOKEN,
                "token text is not a valid slice of the source",
                declaration.name.range(),
            );
            return false;
        };
        if name.is_empty() || declaration.is_const {
            return false;
        }
        let Some(plan) = self.enum_facts.declaration(declaration_id) else {
            self.diag_here(
                codes::ENUM_FACTS_UNAVAILABLE,
                "runtime enum lowering requires checked enum facts",
            );
            return false;
        };
        if plan.members().len() != declaration.members.len() {
            self.diag_here(
                codes::ENUM_FACTS_UNAVAILABLE,
                "runtime enum lowering requires a complete checked enum plan",
            );
            return false;
        }

        self.raw("var ");
        self.raw(&name);
        self.raw(";");
        self.newline();
        self.raw("(function (");
        self.raw(&name);
        self.raw(") {");
        self.newline();
        self.indent += 1;
        for (member, plan_member) in declaration.members.iter().zip(plan.members()) {
            if self.emit_enum_member(&name, member.data(), plan_member) {
                self.newline();
            }
        }
        self.indent -= 1;
        self.raw("})(");
        self.raw(&name);
        self.raw(" || (");
        self.raw(&name);
        self.raw(" = {}));");
        true
    }

    fn emit_enum_member(
        &mut self,
        object: &str,
        member: &EnumMember,
        plan_member: &EnumMemberPlan,
    ) -> bool {
        let (Some(key), Some(value)) = (plan_member.name(), plan_member.value()) else {
            return false;
        };
        let initializer = member.initializer.as_deref();
        if value.constant().is_none() && initializer.is_none() {
            return false;
        }

        self.raw(object);
        self.raw("[");
        if plan_member.reverse() {
            self.raw(object);
            self.raw("[");
        }
        self.emit_enum_string(key);
        self.raw("] = ");
        if let Some(constant) = value.constant() {
            self.emit_enum_scalar(constant);
        } else if let Some(initializer) = initializer {
            self.emit_expression_prec(initializer, P_ASSIGN);
        }
        if plan_member.reverse() {
            self.raw("] = ");
            self.emit_enum_string(key);
        }
        self.raw(";");
        true
    }

    fn emit_enum_scalar(&mut self, scalar: &EnumScalar) {
        match scalar {
            EnumScalar::Number(value) => {
                let value = value.to_f64();
                if value.is_nan() {
                    self.raw("NaN");
                } else if value == f64::INFINITY {
                    self.raw("Infinity");
                } else if value == f64::NEG_INFINITY {
                    self.raw("-Infinity");
                } else {
                    let mut text = String::new();
                    write!(text, "{value}").expect("writing to a String cannot fail");
                    self.raw(&text);
                }
            }
            EnumScalar::String(value) => self.emit_enum_string(value),
        }
    }

    fn emit_enum_string(&mut self, value: &bamts_bytecode::EcmaString) {
        self.raw("\"");
        for &unit in value.as_units() {
            match unit {
                0x08 => self.raw("\\b"),
                0x09 => self.raw("\\t"),
                0x0A => self.raw("\\n"),
                0x0C => self.raw("\\f"),
                0x0D => self.raw("\\r"),
                0x22 => self.raw("\\\""),
                0x5C => self.raw("\\\\"),
                0x20..=0x7E => {
                    let ch = char::from_u32(u32::from(unit)).expect("ASCII unit");
                    let mut buf = [0u8; 4];
                    self.raw(ch.encode_utf8(&mut buf));
                }
                _ => {
                    let mut text = String::new();
                    write!(text, "\\u{unit:04X}").expect("writing to a String cannot fail");
                    self.raw(&text);
                }
            }
        }
        self.raw("\"");
    }

    // =======================================================================
    // functions / classes (JavaScript)
    // =======================================================================

    fn emit_function_declaration_js(&mut self, function: &FunctionLike) {
        if function.is_async {
            self.raw("async ");
        }
        self.raw("function");
        if function.is_generator {
            self.raw("*");
        }
        if let Some(name) = &function.name {
            self.raw(" ");
            self.emit_ident(name);
        }
        self.emit_params_js(&function.parameters);
        self.raw(" ");
        self.emit_function_body_js(function.body.as_ref());
    }

    fn emit_function_expression_js(&mut self, function: &FunctionLike) {
        if function.is_async {
            self.raw("async ");
        }
        self.raw("function");
        if function.is_generator {
            self.raw("*");
        }
        if let Some(name) = &function.name {
            self.raw(" ");
            self.emit_ident(name);
        }
        self.emit_params_js(&function.parameters);
        self.raw(" ");
        self.emit_function_body_js(function.body.as_ref());
    }

    fn emit_arrow_js(&mut self, arrow: &ArrowFunction) {
        if arrow.is_async {
            self.raw("async ");
        }
        self.emit_params_js(&arrow.parameters);
        self.raw(" => ");
        match &arrow.body {
            FunctionBody::Block(block) => self.emit_block(block.data()),
            FunctionBody::Expression(expression) => {
                if self.leads_with_bad_token(expression, true) {
                    self.raw("(");
                    self.emit_expression_prec(expression, 0);
                    self.raw(")");
                } else {
                    self.emit_expression_prec(expression, P_ASSIGN);
                }
            }
            FunctionBody::Missing(_) => {
                self.diag_here(
                    codes::MISSING_STATEMENT,
                    "cannot emit a missing function body",
                );
                self.raw("{}");
            }
        }
    }

    fn emit_function_body_js(&mut self, body: Option<&FunctionBody>) {
        match body {
            Some(FunctionBody::Block(block)) => self.emit_block(block.data()),
            Some(FunctionBody::Expression(expression)) => {
                self.raw("{ return ");
                self.emit_expression_prec(expression, 0);
                self.raw("; }");
            }
            Some(FunctionBody::Missing(_)) | None => {
                self.diag_here(
                    codes::MISSING_STATEMENT,
                    "cannot emit a missing function body",
                );
                self.raw("{}");
            }
        }
    }

    fn emit_params_js(&mut self, parameters: &[ParameterNode]) {
        self.raw("(");
        let mut first = true;
        for parameter in parameters {
            let parameter = parameter.data();
            if self.is_this_parameter(parameter) {
                continue;
            }
            if !first {
                self.raw(", ");
            }
            first = false;
            self.emit_decorators_inline(&parameter.decorators);
            self.emit_pattern(&parameter.binding);
            if let Some(initializer) = &parameter.initializer {
                self.raw(" = ");
                self.emit_expression_prec(initializer, P_ASSIGN);
            }
        }
        self.raw(")");
    }

    fn is_this_parameter(&self, parameter: &Parameter) -> bool {
        if let BindingPattern::Identifier(ident) = parameter.binding.data() {
            matches!(self.text(ident.data().token()), Some("this"))
        } else {
            false
        }
    }

    fn emit_decorator(&mut self, decorator: &DecoratorNode) {
        self.raw("@");
        self.emit_expression_prec(&decorator.data().expression, P_ASSIGN);
    }

    fn emit_decorators_inline(&mut self, decorators: &[DecoratorNode]) {
        for decorator in decorators {
            self.emit_decorator(decorator);
            self.raw(" ");
        }
    }

    fn emit_decorators_block(&mut self, decorators: &[DecoratorNode]) {
        for decorator in decorators {
            self.emit_decorator(decorator);
            self.newline();
        }
    }

    fn emit_class_js(&mut self, class: &ClassDeclaration) {
        self.emit_decorators_block(&class.decorators);
        self.emit_class_core_js(class);
    }

    fn emit_class_core_js(&mut self, class: &ClassDeclaration) {
        self.raw("class");
        if let Some(name) = &class.name {
            self.raw(" ");
            self.emit_ident(name);
        }
        if let Some(heritage) = &class.extends {
            self.raw(" extends ");
            self.emit_expression_prec(&heritage.expression, P_CALL_MEMBER);
        }
        self.raw(" ");
        self.emit_class_body_js(&class.members);
    }

    fn emit_class_body_js(&mut self, members: &[ClassMemberNode]) {
        self.raw("{");
        let has = members
            .iter()
            .any(|member| self.class_member_emits_js(member.data()));
        if has {
            self.newline();
            self.indent += 1;
        }
        for member in members {
            if self.emit_class_member_js(member.data()) {
                self.newline();
            }
        }
        if has {
            self.indent -= 1;
        }
        self.raw("}");
    }

    fn class_member_emits_js(&self, member: &ClassMember) -> bool {
        match member {
            ClassMember::Constructor(_) | ClassMember::StaticBlock(_) => true,
            ClassMember::Method(method) => {
                method.function.body.is_some() && !method.modifiers.is_abstract
            }
            ClassMember::Property(property) => {
                !property.modifiers.is_abstract && !property.modifiers.is_declare
            }
            ClassMember::AutoAccessor(accessor) => {
                !accessor.modifiers.is_abstract && !accessor.modifiers.is_declare
            }
            ClassMember::IndexSignature(_) | ClassMember::Missing(_) => false,
        }
    }

    fn emit_class_member_js(&mut self, member: &ClassMember) -> bool {
        match member {
            ClassMember::Constructor(constructor) => {
                self.emit_decorators_block(&constructor.decorators);
                self.emit_constructor_js(constructor);
                true
            }
            ClassMember::Method(method) => {
                if method.function.body.is_none() || method.modifiers.is_abstract {
                    return false;
                }
                self.emit_decorators_block(&method.function.decorators);
                self.emit_method_js(method);
                true
            }
            ClassMember::Property(property) => {
                if property.modifiers.is_abstract || property.modifiers.is_declare {
                    return false;
                }
                self.emit_decorators_block(&property.decorators);
                if property.modifiers.is_static {
                    self.raw("static ");
                }
                self.emit_property_name(&property.name);
                if let Some(initializer) = &property.initializer {
                    self.raw(" = ");
                    self.emit_expression_prec(initializer, P_ASSIGN);
                }
                self.raw(";");
                true
            }
            ClassMember::AutoAccessor(accessor) => {
                if accessor.modifiers.is_abstract || accessor.modifiers.is_declare {
                    return false;
                }
                self.emit_decorators_block(&accessor.decorators);
                if accessor.modifiers.is_static {
                    self.raw("static ");
                }
                self.raw("accessor ");
                self.emit_property_name(&accessor.name);
                if let Some(initializer) = &accessor.initializer {
                    self.raw(" = ");
                    self.emit_expression_prec(initializer, P_ASSIGN);
                }
                self.raw(";");
                true
            }
            ClassMember::StaticBlock(block) => {
                self.raw("static ");
                self.emit_block(block.data());
                true
            }
            ClassMember::IndexSignature(_) => false,
            ClassMember::Missing(_) => {
                self.diag_here(codes::MISSING_MEMBER, "cannot emit a missing class member");
                false
            }
        }
    }

    fn emit_method_js(&mut self, method: &MethodDeclaration) {
        if method.modifiers.is_static {
            self.raw("static ");
        }
        match method.modifier {
            PropertyModifier::Get => self.raw("get "),
            PropertyModifier::Set => self.raw("set "),
            PropertyModifier::None => {
                if method.function.is_async {
                    self.raw("async ");
                }
                if method.function.is_generator {
                    self.raw("*");
                }
            }
        }
        self.emit_property_name(&method.name);
        self.emit_params_js(&method.function.parameters);
        self.raw(" ");
        self.emit_function_body_js(method.function.body.as_ref());
    }

    fn emit_constructor_js(&mut self, constructor: &ConstructorDeclaration) {
        self.raw("constructor");
        self.emit_params_js(&constructor.parameters);
        self.raw(" ");

        let injections: Vec<&ParameterNode> = constructor
            .parameters
            .iter()
            .filter(|parameter| is_parameter_property(parameter.data()))
            .collect();
        let body = constructor.body.data();
        if injections.is_empty() && body.statements.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        for parameter in injections {
            if let BindingPattern::Identifier(name) = parameter.data().binding.data() {
                self.raw("this.");
                self.emit_ident(name);
                self.raw(" = ");
                self.emit_ident(name);
                self.raw(";");
                self.newline();
            } else {
                self.diag(
                    codes::MISSING_TARGET,
                    "parameter property must bind a plain identifier",
                    parameter.range(),
                );
            }
        }
        for statement in &body.statements {
            if self.emit_statement(statement) {
                self.newline();
            }
        }
        self.indent -= 1;
        self.raw("}");
    }

    // =======================================================================
    // expressions (precedence-aware)
    // =======================================================================

    fn emit_expression(&mut self, expression: &Expr) {
        self.emit_expression_prec(expression, 0);
    }

    fn emit_expression_prec(&mut self, expression: &Expr, min_prec: u8) {
        let previous = self.anchor;
        self.anchor = expression.range();
        let prec = self.expression_prec(expression);
        let parenthesize = prec < min_prec;
        if parenthesize {
            self.raw("(");
        }
        self.emit_expression_inner(expression);
        if parenthesize {
            self.raw(")");
        }
        self.anchor = previous;
    }

    fn expression_prec(&self, expression: &Expr) -> u8 {
        if let Some(value) = self.enum_facts.const_use(expression.id()) {
            return match value.number() {
                Some(number) if number.to_f64().is_sign_negative() => P_UNARY,
                _ => P_PRIMARY,
            };
        }
        match expression.data() {
            Expression::Sequence(_) => P_SEQUENCE,
            Expression::Assignment(_) | Expression::Arrow(_) | Expression::Yield(_) => P_ASSIGN,
            Expression::Conditional(_) => P_CONDITIONAL,
            Expression::Logical(logical) => match logical.operator {
                LogicalOperator::And => P_LOGICAL_AND,
                LogicalOperator::Or => P_LOGICAL_OR,
                LogicalOperator::Nullish => P_NULLISH,
            },
            Expression::Binary(binary) => binary_prec(binary.operator),
            Expression::Unary(_) | Expression::Await(_) => P_UNARY,
            Expression::Update(update) => {
                if update.prefix {
                    P_UNARY
                } else {
                    P_POSTFIX
                }
            }
            Expression::Call(_)
            | Expression::New(_)
            | Expression::Member(_)
            | Expression::TaggedTemplate(_)
            | Expression::Import(_) => P_CALL_MEMBER,
            Expression::Parenthesized(inner) => self.expression_prec(inner),
            Expression::As(as_expr) => self.expression_prec(&as_expr.expression),
            Expression::Satisfies(satisfies) => self.expression_prec(&satisfies.expression),
            Expression::TypeAssertion(assertion) => self.expression_prec(&assertion.expression),
            Expression::NonNull(non_null) => self.expression_prec(&non_null.expression),
            _ => P_PRIMARY,
        }
    }

    fn emit_expression_inner(&mut self, expression: &Expr) {
        if let Some(value) = self.enum_facts.const_use(expression.id()) {
            self.emit_enum_scalar(value);
            return;
        }
        match expression.data() {
            Expression::JsxElement(_)
            | Expression::JsxFragment(_)
            | Expression::JsxSelfClosingElement(_) => {
                self.diag_here(
                    codes::MISSING_EXPRESSION,
                    "cannot emit a JSX expression node",
                );
                self.raw("void 0");
            }
            Expression::Identifier(ident) => self.emit_ident_expression(ident),
            Expression::This => self.raw("this"),
            Expression::Super => self.raw("super"),
            Expression::Literal(literal) => self.emit_literal(literal),
            Expression::Template(template) => self.emit_template(template),
            Expression::TaggedTemplate(tagged) => {
                self.emit_expression_prec(&tagged.tag, P_CALL_MEMBER);
                self.emit_template(&tagged.template);
            }
            Expression::Array(array) => self.emit_array(array),
            Expression::Object(object) => self.emit_object(object),
            Expression::Function(function) => self.emit_function_expression_js(&function.function),
            Expression::Class(class) => self.emit_class_js(&class.class),
            Expression::Arrow(arrow) => self.emit_arrow_js(arrow),
            Expression::Call(call) => self.emit_call(call),
            Expression::Member(member) => self.emit_member(member),
            Expression::New(new) => self.emit_new(new),
            Expression::Await(await_expr) => {
                self.raw("await ");
                self.emit_expression_prec(&await_expr.argument, P_UNARY);
            }
            Expression::Yield(yield_expr) => self.emit_yield(yield_expr),
            Expression::Unary(unary) => self.emit_unary(unary),
            Expression::Update(update) => self.emit_update(update),
            Expression::Binary(binary) => self.emit_binary(binary),
            Expression::Logical(logical) => self.emit_logical(logical),
            Expression::Conditional(conditional) => self.emit_conditional(conditional),
            Expression::Assignment(assignment) => self.emit_assignment(assignment),
            Expression::Sequence(sequence) => self.emit_sequence(sequence),
            Expression::Parenthesized(inner) => self.emit_expression_prec(inner, 0),
            Expression::As(as_expr) => self.emit_expression_prec(&as_expr.expression, 0),
            Expression::Satisfies(satisfies) => {
                self.emit_expression_prec(&satisfies.expression, 0);
            }
            Expression::TypeAssertion(assertion) => {
                self.emit_expression_prec(&assertion.expression, 0);
            }
            Expression::NonNull(non_null) => self.emit_expression_prec(&non_null.expression, 0),
            Expression::Import(import) => {
                self.raw("import(");
                self.emit_expression_prec(&import.source, P_ASSIGN);
                if let Some(options) = &import.options {
                    self.raw(", ");
                    self.emit_expression_prec(options, P_ASSIGN);
                }
                self.raw(")");
            }
            Expression::Meta(meta) => match meta {
                MetaProperty::NewTarget => self.raw("new.target"),
                MetaProperty::ImportMeta => self.raw("import.meta"),
            },
            Expression::Missing(_) => {
                self.diag_here(
                    codes::MISSING_EXPRESSION,
                    "cannot emit a missing expression node",
                );
                self.raw("void 0");
            }
        }
    }

    fn emit_ident_expression(&mut self, ident: &IdentifierNode) {
        let Some(text) = self.text(ident.data().token()) else {
            let range = ident.range();
            self.diag(
                codes::UNRESOLVED_TOKEN,
                "token text is not a valid slice of the source",
                range,
            );
            return;
        };
        if let Some(member) = self.enum_facts.member_use(ident.id()) {
            let enum_name = self.model.symbol(member.enum_symbol()).name().to_owned();
            self.raw(&enum_name);
            self.raw("[");
            self.emit_enum_string(member.name());
            self.raw("]");
            return;
        }
        self.raw(text);
    }

    fn emit_literal(&mut self, literal: &Literal) {
        match literal {
            Literal::String(node) => self.emit_token(node.data().token()),
            Literal::Number(node) => self.emit_token(node.data().token()),
            Literal::BigInt(node) => self.emit_token(node.data().token()),
            Literal::Boolean(node) => self.emit_token(node.data().token()),
            Literal::Null(node) => match self.text(node.data().token()) {
                Some(text) if !text.is_empty() => self.raw(text),
                _ => self.raw("null"),
            },
            Literal::Regex(node) => self.emit_token(node.data().token()),
        }
    }

    fn emit_template(&mut self, template: &TemplateLiteral) {
        if template.elements.is_empty() {
            return;
        }
        self.emit_token(template.elements[0].data().token());
        for (index, expression) in template.expressions.iter().enumerate() {
            self.emit_expression_prec(expression, 0);
            if let Some(element) = template.elements.get(index + 1) {
                self.emit_token(element.data().token());
            }
        }
    }

    fn emit_array(&mut self, array: &ArrayLiteral) {
        self.raw("[");
        let elements = &array.elements;
        for (index, element) in elements.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            match element {
                ArrayElement::Expression(expression) => {
                    self.emit_expression_prec(expression, P_ASSIGN);
                }
                ArrayElement::Spread(spread) => {
                    self.raw("...");
                    self.emit_expression_prec(&spread.argument, P_ASSIGN);
                }
                ArrayElement::Elision => {}
                ArrayElement::Missing(_) => {
                    self.diag_here(
                        codes::MISSING_ELEMENT,
                        "cannot emit a missing array element",
                    );
                }
            }
        }
        if matches!(elements.last(), Some(ArrayElement::Elision)) {
            self.raw(",");
        }
        self.raw("]");
    }

    fn emit_object(&mut self, object: &ObjectLiteral) {
        if object.members.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        let count = object.members.len();
        for (index, member) in object.members.iter().enumerate() {
            self.emit_object_member(member.data());
            if index + 1 < count {
                self.raw(",");
            }
            self.newline();
        }
        self.indent -= 1;
        self.raw("}");
    }

    fn emit_object_member(&mut self, member: &ObjectMember) {
        match member {
            ObjectMember::Property(property) => {
                if property.shorthand && matches!(property.name, PropertyName::Identifier(_)) {
                    self.emit_property_name(&property.name);
                } else {
                    self.emit_property_name(&property.name);
                    self.raw(": ");
                    self.emit_expression_prec(&property.value, P_ASSIGN);
                }
            }
            ObjectMember::Method(method) => self.emit_object_method(method),
            ObjectMember::Spread(spread) => {
                self.raw("...");
                self.emit_expression_prec(&spread.argument, P_ASSIGN);
            }
            ObjectMember::Missing(_) => {
                self.diag_here(codes::MISSING_MEMBER, "cannot emit a missing object member");
            }
        }
    }

    fn emit_object_method(&mut self, method: &ObjectMethod) {
        match method.modifier {
            PropertyModifier::Get => self.raw("get "),
            PropertyModifier::Set => self.raw("set "),
            PropertyModifier::None => {
                if method.function.is_async {
                    self.raw("async ");
                }
                if method.function.is_generator {
                    self.raw("*");
                }
            }
        }
        self.emit_property_name(&method.name);
        self.emit_params_js(&method.function.parameters);
        self.raw(" ");
        self.emit_function_body_js(method.function.body.as_ref());
    }

    fn emit_property_name(&mut self, name: &PropertyName) {
        match name {
            PropertyName::Identifier(ident) => self.emit_ident(ident),
            PropertyName::Private(private) => self.emit_token(private.data().token()),
            PropertyName::String(string) => self.emit_string(string),
            PropertyName::Number(number) => self.emit_token(number.data().token()),
            PropertyName::Computed(expression) => {
                self.raw("[");
                self.emit_expression_prec(expression, P_ASSIGN);
                self.raw("]");
            }
            PropertyName::Missing(_) => {
                self.diag_here(
                    codes::MISSING_PROPERTY_NAME,
                    "cannot emit a missing property name",
                );
            }
        }
    }

    fn emit_call(&mut self, call: &CallExpression) {
        self.emit_expression_prec(&call.callee, P_CALL_MEMBER);
        if call.optional {
            self.raw("?.(");
        } else {
            self.raw("(");
        }
        self.emit_arguments(&call.arguments);
        self.raw(")");
    }

    fn emit_new(&mut self, new: &NewExpression) {
        self.raw("new ");
        let callee = self.unwrap_expression(&new.callee);
        let wrap = matches!(callee.data(), Expression::Call(_)) || self.chain_has_optional(callee);
        if wrap {
            self.raw("(");
            self.emit_expression_prec(callee, 0);
            self.raw(")");
        } else {
            self.emit_expression_prec(callee, P_CALL_MEMBER);
        }
        self.raw("(");
        self.emit_arguments(&new.arguments);
        self.raw(")");
    }

    fn emit_arguments(&mut self, arguments: &[CallArgument]) {
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            match argument {
                CallArgument::Expression(expression) => {
                    self.emit_expression_prec(expression, P_ASSIGN);
                }
                CallArgument::Spread(spread) => {
                    self.raw("...");
                    self.emit_expression_prec(&spread.argument, P_ASSIGN);
                }
                CallArgument::Missing(_) => {
                    self.diag_here(
                        codes::MISSING_ELEMENT,
                        "cannot emit a missing call argument",
                    );
                }
            }
        }
    }

    fn emit_member(&mut self, member: &MemberExpression) {
        let dotted = !matches!(member.property, MemberProperty::Computed(_));
        let numeric_object = dotted
            && matches!(
                self.unwrap_expression(&member.object).data(),
                Expression::Literal(Literal::Number(_))
            );
        if numeric_object {
            self.raw("(");
            self.emit_expression_prec(&member.object, 0);
            self.raw(")");
        } else {
            self.emit_expression_prec(&member.object, P_CALL_MEMBER);
        }
        match &member.property {
            MemberProperty::Named(name) => {
                self.raw(if member.optional { "?." } else { "." });
                self.emit_ident(name);
            }
            MemberProperty::Private(private) => {
                self.raw(if member.optional { "?." } else { "." });
                self.emit_token(private.data().token());
            }
            MemberProperty::Computed(expression) => {
                self.raw(if member.optional { "?.[" } else { "[" });
                self.emit_expression_prec(expression, 0);
                self.raw("]");
            }
        }
    }

    fn emit_yield(&mut self, expression: &YieldExpression) {
        self.raw("yield");
        if expression.delegate {
            self.raw("*");
        }
        if let Some(argument) = &expression.argument {
            self.raw(" ");
            self.emit_expression_prec(argument, P_ASSIGN);
        }
    }

    fn emit_unary(&mut self, unary: &UnaryExpression) {
        match unary.operator {
            UnaryOperator::Typeof => self.raw("typeof "),
            UnaryOperator::Void => self.raw("void "),
            UnaryOperator::Delete => self.raw("delete "),
            UnaryOperator::Plus => self.raw("+"),
            UnaryOperator::Minus => self.raw("-"),
            UnaryOperator::Not => self.raw("!"),
            UnaryOperator::BitNot => self.raw("~"),
        }
        if matches!(unary.operator, UnaryOperator::Plus | UnaryOperator::Minus)
            && self.needs_unary_space(&unary.argument)
        {
            self.raw(" ");
        }
        self.emit_expression_prec(&unary.argument, P_UNARY);
    }

    fn needs_unary_space(&self, argument: &Expr) -> bool {
        match self.unwrap_expression(argument).data() {
            Expression::Unary(inner) => {
                matches!(inner.operator, UnaryOperator::Plus | UnaryOperator::Minus)
            }
            Expression::Update(inner) => inner.prefix,
            _ => false,
        }
    }

    fn emit_update(&mut self, update: &UpdateExpression) {
        let operator = match update.operator {
            UpdateOperator::Increment => "++",
            UpdateOperator::Decrement => "--",
        };
        if update.prefix {
            self.raw(operator);
            self.emit_assignment_target(&update.argument);
        } else {
            self.emit_assignment_target(&update.argument);
            self.raw(operator);
        }
    }

    fn emit_binary(&mut self, binary: &BinaryExpression) {
        let prec = binary_prec(binary.operator);
        if matches!(binary.operator, BinaryOperator::Exponentiate) {
            let left = self.unwrap_expression(&binary.left);
            if matches!(left.data(), Expression::Unary(_) | Expression::Await(_))
                || matches!(left.data(), Expression::Update(update) if update.prefix)
            {
                self.raw("(");
                self.emit_expression_prec(left, 0);
                self.raw(")");
            } else {
                self.emit_expression_prec(&binary.left, P_EXPONENT + 1);
            }
            self.raw(" ** ");
            self.emit_expression_prec(&binary.right, P_EXPONENT);
        } else {
            self.emit_expression_prec(&binary.left, prec);
            self.raw(" ");
            self.raw(binary_str(binary.operator));
            self.raw(" ");
            self.emit_expression_prec(&binary.right, prec + 1);
        }
    }

    fn emit_logical(&mut self, logical: &LogicalExpression) {
        let prec = match logical.operator {
            LogicalOperator::And => P_LOGICAL_AND,
            LogicalOperator::Or => P_LOGICAL_OR,
            LogicalOperator::Nullish => P_NULLISH,
        };
        self.emit_logical_operand(&logical.left, logical.operator, prec, true);
        self.raw(" ");
        self.raw(logical_str(logical.operator));
        self.raw(" ");
        self.emit_logical_operand(&logical.right, logical.operator, prec, false);
    }

    fn emit_logical_operand(
        &mut self,
        operand: &Expr,
        parent: LogicalOperator,
        prec: u8,
        is_left: bool,
    ) {
        if self.coalesce_mix(parent, operand) {
            self.raw("(");
            self.emit_expression_prec(operand, 0);
            self.raw(")");
            return;
        }
        let min_prec = if is_left { prec } else { prec + 1 };
        self.emit_expression_prec(operand, min_prec);
    }

    fn coalesce_mix(&self, parent: LogicalOperator, operand: &Expr) -> bool {
        if let Expression::Logical(child) = self.unwrap_expression(operand).data() {
            let parent_nullish = matches!(parent, LogicalOperator::Nullish);
            let child_nullish = matches!(child.operator, LogicalOperator::Nullish);
            parent_nullish != child_nullish
        } else {
            false
        }
    }

    fn emit_conditional(&mut self, conditional: &ConditionalExpression) {
        self.emit_expression_prec(&conditional.test, P_CONDITIONAL + 1);
        self.raw(" ? ");
        self.emit_expression_prec(&conditional.consequent, P_ASSIGN);
        self.raw(" : ");
        self.emit_expression_prec(&conditional.alternate, P_ASSIGN);
    }

    fn emit_assignment(&mut self, assignment: &AssignmentExpression) {
        self.emit_assignment_target(&assignment.left);
        self.raw(" ");
        self.raw(assignment_str(assignment.operator));
        self.raw(" ");
        self.emit_expression_prec(&assignment.right, P_ASSIGN);
    }

    fn emit_sequence(&mut self, sequence: &SequenceExpression) {
        for (index, expression) in sequence.expressions.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            self.emit_expression_prec(expression, P_ASSIGN);
        }
    }

    // ---- assignment targets / patterns ------------------------------------

    fn emit_assignment_target(&mut self, target: &AssignmentTargetNode) {
        match target.data() {
            AssignmentTarget::Identifier(ident) => self.emit_ident(ident),
            AssignmentTarget::Member(member) => {
                let dotted = !matches!(member.property, MemberProperty::Computed(_));
                let numeric_object = dotted
                    && matches!(
                        self.unwrap_expression(&member.object).data(),
                        Expression::Literal(Literal::Number(_))
                    );
                if numeric_object {
                    self.raw("(");
                    self.emit_expression_prec(&member.object, 0);
                    self.raw(")");
                } else {
                    self.emit_expression_prec(&member.object, P_CALL_MEMBER);
                }
                match &member.property {
                    MemberProperty::Named(name) => {
                        self.raw(".");
                        self.emit_ident(name);
                    }
                    MemberProperty::Private(private) => {
                        self.raw(".");
                        self.emit_token(private.data().token());
                    }
                    MemberProperty::Computed(expression) => {
                        self.raw("[");
                        self.emit_expression_prec(expression, 0);
                        self.raw("]");
                    }
                }
            }
            AssignmentTarget::Object(pattern) => {
                self.raw("{");
                for (index, property) in pattern.properties.iter().enumerate() {
                    if index > 0 {
                        self.raw(", ");
                    } else {
                        self.raw(" ");
                    }
                    self.emit_assignment_object_property(property);
                }
                if pattern.properties.is_empty() {
                    self.raw("}");
                } else {
                    self.raw(" }");
                }
            }
            AssignmentTarget::Array(pattern) => {
                self.raw("[");
                for (index, element) in pattern.elements.iter().enumerate() {
                    if index > 0 {
                        self.raw(", ");
                    }
                    match element {
                        AssignmentArrayElement::Target(target) => {
                            self.emit_assignment_target(target)
                        }
                        AssignmentArrayElement::Elision => {}
                        AssignmentArrayElement::Missing(_) => {
                            self.diag_here(codes::MISSING_TARGET, "cannot emit a missing target");
                        }
                    }
                }
                if matches!(
                    pattern.elements.last(),
                    Some(AssignmentArrayElement::Elision)
                ) {
                    self.raw(",");
                }
                self.raw("]");
            }
            AssignmentTarget::Missing(_) => {
                self.diag_here(
                    codes::MISSING_TARGET,
                    "cannot emit a missing assignment target",
                );
            }
        }
    }

    fn emit_assignment_object_property(&mut self, property: &AssignmentObjectProperty) {
        let shorthand = property.initializer.is_none()
            && self.target_matches_name(&property.name, &property.target);
        if shorthand {
            self.emit_property_name(&property.name);
        } else {
            self.emit_property_name(&property.name);
            self.raw(": ");
            self.emit_assignment_target(&property.target);
        }
        if let Some(initializer) = &property.initializer {
            self.raw(" = ");
            self.emit_expression_prec(initializer, P_ASSIGN);
        }
    }

    fn emit_pattern(&mut self, pattern: &Pattern) {
        match pattern.data() {
            BindingPattern::Identifier(ident) => self.emit_ident(ident),
            BindingPattern::Object(object) => {
                if object.properties.is_empty() {
                    self.raw("{}");
                    return;
                }
                self.raw("{ ");
                for (index, property) in object.properties.iter().enumerate() {
                    if index > 0 {
                        self.raw(", ");
                    }
                    self.emit_object_binding_property(property);
                }
                self.raw(" }");
            }
            BindingPattern::Array(array) => {
                self.raw("[");
                for (index, element) in array.elements.iter().enumerate() {
                    if index > 0 {
                        self.raw(", ");
                    }
                    match element {
                        ArrayBindingElement::Binding(binding) => self.emit_pattern(binding),
                        ArrayBindingElement::Elision => {}
                        ArrayBindingElement::Missing(_) => {
                            self.diag_here(codes::MISSING_BINDING, "cannot emit a missing binding");
                        }
                    }
                }
                if matches!(array.elements.last(), Some(ArrayBindingElement::Elision)) {
                    self.raw(",");
                }
                self.raw("]");
            }
            BindingPattern::Rest(rest) => {
                self.raw("...");
                self.emit_pattern(&rest.argument);
            }
            BindingPattern::Assignment(assignment) => {
                self.emit_pattern(&assignment.left);
                self.raw(" = ");
                self.emit_expression_prec(&assignment.right, P_ASSIGN);
            }
            BindingPattern::Missing(_) => {
                self.diag_here(
                    codes::MISSING_BINDING,
                    "cannot emit a missing binding pattern",
                );
            }
        }
    }

    fn emit_object_binding_property(&mut self, property: &ObjectBindingProperty) {
        if let BindingPattern::Rest(rest) = property.binding.data() {
            self.raw("...");
            self.emit_pattern(&rest.argument);
            return;
        }
        let shorthand = self.binding_matches_name(&property.name, &property.binding);
        if shorthand {
            self.emit_property_name(&property.name);
        } else {
            self.emit_property_name(&property.name);
            self.raw(": ");
            self.emit_pattern(&property.binding);
        }
        if let Some(initializer) = &property.initializer {
            self.raw(" = ");
            self.emit_expression_prec(initializer, P_ASSIGN);
        }
    }

    // ---- shorthand / name matching helpers --------------------------------

    fn module_name_matches_ident(&self, name: &ModuleExportName, ident: &IdentifierNode) -> bool {
        match name {
            ModuleExportName::Identifier(imported) => self.same_ident_text(imported, ident),
            _ => false,
        }
    }

    fn module_names_match(&self, left: &ModuleExportName, right: &ModuleExportName) -> bool {
        match (left, right) {
            (ModuleExportName::Identifier(a), ModuleExportName::Identifier(b)) => {
                self.same_ident_text(a, b)
            }
            _ => false,
        }
    }

    fn target_matches_name(&self, name: &PropertyName, target: &AssignmentTargetNode) -> bool {
        if let (PropertyName::Identifier(a), AssignmentTarget::Identifier(b)) =
            (name, target.data())
        {
            self.same_ident_text(a, b)
        } else {
            false
        }
    }

    fn binding_matches_name(&self, name: &PropertyName, binding: &Pattern) -> bool {
        if let (PropertyName::Identifier(a), BindingPattern::Identifier(b)) = (name, binding.data())
        {
            self.same_ident_text(a, b)
        } else {
            false
        }
    }

    fn same_ident_text(&self, left: &IdentifierNode, right: &IdentifierNode) -> bool {
        match (
            self.text(left.data().token()),
            self.text(right.data().token()),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    fn emit_module_export_name(&mut self, name: &ModuleExportName) {
        match name {
            ModuleExportName::Identifier(ident) => self.emit_ident(ident),
            ModuleExportName::String(string) => self.emit_string(string),
            ModuleExportName::Missing(_) => {
                self.diag_here(
                    codes::MISSING_NAME,
                    "cannot emit a missing module export name",
                );
            }
        }
    }

    fn emit_entity_name(&mut self, name: &EntityName) {
        match name {
            EntityName::Identifier(ident) => self.emit_ident(ident),
            EntityName::Qualified { left, right } => {
                self.emit_entity_name(left);
                self.raw(".");
                self.emit_ident(right);
            }
            EntityName::Missing(_) => {
                self.diag_here(codes::MISSING_NAME, "cannot emit a missing entity name");
            }
        }
    }

    // ---- expression classification ----------------------------------------

    fn unwrap_expression<'e>(&self, expression: &'e Expr) -> &'e Expr {
        match expression.data() {
            Expression::Parenthesized(inner) => self.unwrap_expression(inner),
            Expression::As(as_expr) => self.unwrap_expression(&as_expr.expression),
            Expression::Satisfies(satisfies) => self.unwrap_expression(&satisfies.expression),
            Expression::TypeAssertion(assertion) => self.unwrap_expression(&assertion.expression),
            Expression::NonNull(non_null) => self.unwrap_expression(&non_null.expression),
            _ => expression,
        }
    }

    fn chain_has_optional(&self, expression: &Expr) -> bool {
        match expression.data() {
            Expression::Member(member) => {
                member.optional || self.chain_has_optional(&member.object)
            }
            Expression::Call(call) => call.optional || self.chain_has_optional(&call.callee),
            Expression::NonNull(non_null) => self.chain_has_optional(&non_null.expression),
            Expression::Parenthesized(inner) => self.chain_has_optional(inner),
            Expression::As(as_expr) => self.chain_has_optional(&as_expr.expression),
            Expression::Satisfies(satisfies) => self.chain_has_optional(&satisfies.expression),
            Expression::TypeAssertion(assertion) => self.chain_has_optional(&assertion.expression),
            _ => false,
        }
    }

    /// Whether an expression, at a statement or arrow-body position, would begin
    /// with a token requiring wrapping parentheses (`{`, `function`, `class`).
    fn leads_with_bad_token(&self, expression: &Expr, objects_only: bool) -> bool {
        match expression.data() {
            Expression::Object(_) => true,
            Expression::Function(_) | Expression::Class(_) => !objects_only,
            Expression::Binary(binary) => self.leads_with_bad_token(&binary.left, objects_only),
            Expression::Logical(logical) => self.leads_with_bad_token(&logical.left, objects_only),
            Expression::Conditional(conditional) => {
                self.leads_with_bad_token(&conditional.test, objects_only)
            }
            Expression::Assignment(assignment) => self.target_leads_with_object(&assignment.left),
            Expression::Sequence(sequence) => sequence
                .expressions
                .first()
                .is_some_and(|first| self.leads_with_bad_token(first, objects_only)),
            Expression::Call(call) => self.leads_with_bad_token(&call.callee, objects_only),
            Expression::Member(member) => self.leads_with_bad_token(&member.object, objects_only),
            Expression::TaggedTemplate(tagged) => {
                self.leads_with_bad_token(&tagged.tag, objects_only)
            }
            Expression::Update(update) => {
                !update.prefix && self.target_leads_with_object(&update.argument)
            }
            Expression::NonNull(non_null) => {
                self.leads_with_bad_token(&non_null.expression, objects_only)
            }
            Expression::As(as_expr) => self.leads_with_bad_token(&as_expr.expression, objects_only),
            Expression::Satisfies(satisfies) => {
                self.leads_with_bad_token(&satisfies.expression, objects_only)
            }
            Expression::TypeAssertion(assertion) => {
                self.leads_with_bad_token(&assertion.expression, objects_only)
            }
            Expression::Parenthesized(inner) => self.leads_with_bad_token(inner, objects_only),
            _ => false,
        }
    }

    fn target_leads_with_object(&self, target: &AssignmentTargetNode) -> bool {
        match target.data() {
            AssignmentTarget::Object(_) => true,
            AssignmentTarget::Member(member) => self.leads_with_bad_token(&member.object, false),
            _ => false,
        }
    }

    // =======================================================================
    // Declaration (.d.ts) emit
    // =======================================================================

    fn emit_declaration(&mut self, statement: &Stmt) -> bool {
        let previous = self.anchor;
        self.anchor = statement.range();
        let emitted = match statement.data() {
            Statement::Import(import) => self.emit_import(import, true),
            Statement::ImportEquals(import) => {
                self.emit_import_equals_decl(import);
                true
            }
            Statement::Export(export) => self.emit_export_decl(export),
            Statement::Variable(declaration) => {
                self.emit_variable_decl(declaration);
                true
            }
            Statement::Function(function) => {
                self.emit_declare_prefix();
                self.emit_function_signature_decl(&function.function);
                true
            }
            Statement::Class(class) => {
                self.emit_declare_prefix();
                self.emit_class_core_decl(class);
                true
            }
            Statement::Interface(interface) => {
                self.emit_interface_decl(interface);
                true
            }
            Statement::TypeAlias(alias) => {
                self.emit_type_alias_decl(alias);
                true
            }
            Statement::Enum(declaration) => {
                self.emit_enum_decl(declaration);
                true
            }
            Statement::Namespace(namespace) => {
                self.emit_namespace_decl(namespace);
                true
            }
            Statement::Declare(inner) => self.emit_declaration(inner),
            Statement::Missing(_) => {
                self.diag_here(
                    codes::MISSING_STATEMENT,
                    "cannot emit a missing statement node",
                );
                false
            }
            _ => false,
        };
        self.anchor = previous;
        emitted
    }

    fn decl_statement_emits(&self, statement: &Stmt) -> bool {
        matches!(
            statement.data(),
            Statement::Import(_)
                | Statement::ImportEquals(_)
                | Statement::Variable(_)
                | Statement::Function(_)
                | Statement::Class(_)
                | Statement::Interface(_)
                | Statement::TypeAlias(_)
                | Statement::Enum(_)
                | Statement::Namespace(_)
                | Statement::Declare(_)
        )
    }

    fn emit_declare_prefix(&mut self) {
        if self.decl_ambient && !self.decl_in_export {
            self.raw("declare ");
        }
    }

    fn emit_import_equals_decl(&mut self, import: &ImportEqualsDeclaration) {
        self.emit_declare_prefix();
        self.raw("import ");
        if import.is_type_only {
            self.raw("type ");
        }
        self.emit_ident(&import.local);
        self.raw(" = ");
        match &import.reference {
            ExternalModuleReference::Require(source) => {
                self.raw("require(");
                self.emit_string(source);
                self.raw(")");
            }
            ExternalModuleReference::Qualified(name) => self.emit_entity_name(name),
            ExternalModuleReference::Missing(_) => {
                self.diag_here(
                    codes::MISSING_NAME,
                    "cannot emit a missing module reference",
                );
            }
        }
        self.raw(";");
    }

    fn emit_export_decl(&mut self, export: &ExportDeclaration) -> bool {
        match export {
            ExportDeclaration::Named(ExportNamedDeclaration::Declaration(inner)) => {
                if !self.decl_statement_emits(inner) {
                    return false;
                }
                self.raw("export ");
                self.decl_in_export = true;
                let emitted = self.emit_declaration(inner);
                self.decl_in_export = false;
                emitted
            }
            ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                type_only,
                specifiers,
                source,
                attributes,
            }) => {
                self.raw("export ");
                if *type_only {
                    self.raw("type ");
                }
                if specifiers.is_empty() {
                    self.raw("{}");
                } else {
                    self.raw("{ ");
                    for (index, specifier) in specifiers.iter().enumerate() {
                        if index > 0 {
                            self.raw(", ");
                        }
                        self.emit_export_specifier(specifier.data(), !*type_only);
                    }
                    self.raw(" }");
                }
                if let Some(source) = source {
                    self.raw(" from ");
                    self.emit_string(source);
                    if let Some(attributes) = attributes {
                        self.emit_import_attributes(attributes);
                    }
                }
                self.raw(";");
                true
            }
            ExportDeclaration::All(all) => {
                self.raw("export ");
                if all.type_only {
                    self.raw("type ");
                }
                self.raw("* ");
                if let Some(name) = &all.exported {
                    self.raw("as ");
                    self.emit_module_export_name(name);
                    self.raw(" ");
                }
                self.raw("from ");
                self.emit_string(&all.source);
                if let Some(attributes) = &all.attributes {
                    self.emit_import_attributes(attributes);
                }
                self.raw(";");
                true
            }
            ExportDeclaration::Default(default) => match &default.value {
                ExportDefaultValue::Function(function) => {
                    self.raw("export default ");
                    self.emit_function_signature_decl(function);
                    true
                }
                ExportDefaultValue::Class(class) => {
                    self.raw("export default ");
                    self.emit_class_core_decl(class);
                    true
                }
                ExportDefaultValue::Expression(expression) => {
                    self.raw("export default ");
                    self.emit_expression_prec(expression, P_ASSIGN);
                    self.raw(";");
                    true
                }
                ExportDefaultValue::Missing(_) => {
                    self.diag_here(
                        codes::MISSING_EXPRESSION,
                        "cannot emit a missing default export",
                    );
                    false
                }
                ExportDefaultValue::Interface(interface) => {
                    self.raw("export default ");
                    self.emit_interface_decl(interface);
                    true
                }
            },
            ExportDeclaration::Assignment(expression) => {
                self.raw("export = ");
                self.emit_expression_prec(expression, P_ASSIGN);
                self.raw(";");
                true
            }
        }
    }

    fn emit_variable_decl(&mut self, declaration: &VariableDeclaration) {
        self.emit_declare_prefix();
        self.raw(variable_kind_str(declaration.kind));
        self.raw(" ");
        for (index, declarator) in declaration.declarations.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            let declarator = declarator.data();
            self.emit_pattern(&declarator.binding);
            if let Some(annotation) = &declarator.type_annotation {
                self.raw(": ");
                self.emit_type(&annotation.data().type_node);
            }
        }
        self.raw(";");
    }

    fn emit_function_signature_decl(&mut self, function: &FunctionLike) {
        self.raw("function");
        if let Some(name) = &function.name {
            self.raw(" ");
            self.emit_ident(name);
        }
        self.emit_type_parameters(&function.type_parameters);
        self.emit_params_decl(&function.parameters);
        if let Some(return_type) = &function.return_type {
            self.raw(": ");
            self.emit_type(&return_type.data().type_node);
        }
        self.raw(";");
    }

    fn emit_params_decl(&mut self, parameters: &[ParameterNode]) {
        self.raw("(");
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            let parameter = parameter.data();
            if let Some(accessibility) = parameter.modifiers.accessibility {
                self.raw(accessibility_str(accessibility));
                self.raw(" ");
            }
            if parameter.modifiers.is_readonly {
                self.raw("readonly ");
            }
            self.emit_pattern(&parameter.binding);
            if parameter.optional {
                self.raw("?");
            }
            if let Some(annotation) = &parameter.type_annotation {
                self.raw(": ");
                self.emit_type(&annotation.data().type_node);
            }
        }
        self.raw(")");
    }

    fn emit_class_core_decl(&mut self, class: &ClassDeclaration) {
        if class.modifiers.is_abstract {
            self.raw("abstract ");
        }
        self.raw("class");
        if let Some(name) = &class.name {
            self.raw(" ");
            self.emit_ident(name);
        }
        self.emit_type_parameters(&class.type_parameters);
        if let Some(heritage) = &class.extends {
            self.raw(" extends ");
            self.emit_expression_prec(&heritage.expression, P_CALL_MEMBER);
            self.emit_type_arguments(&heritage.type_arguments);
        }
        if !class.implements.is_empty() {
            self.raw(" implements ");
            for (index, interface) in class.implements.iter().enumerate() {
                if index > 0 {
                    self.raw(", ");
                }
                self.emit_type(interface);
            }
        }
        self.raw(" ");
        self.emit_class_body_decl(&class.members);
    }

    fn emit_class_body_decl(&mut self, members: &[ClassMemberNode]) {
        self.raw("{");
        let has = members
            .iter()
            .any(|member| self.class_member_emits_decl(member.data()));
        if has {
            self.newline();
            self.indent += 1;
        }
        for member in members {
            if self.emit_class_member_decl(member.data()) {
                self.newline();
            }
        }
        if has {
            self.indent -= 1;
        }
        self.raw("}");
    }

    fn class_member_emits_decl(&self, member: &ClassMember) -> bool {
        !matches!(
            member,
            ClassMember::StaticBlock(_) | ClassMember::Missing(_)
        )
    }

    fn emit_class_member_decl(&mut self, member: &ClassMember) -> bool {
        match member {
            ClassMember::Constructor(constructor) => {
                if let Some(accessibility) = constructor.modifiers.accessibility {
                    self.raw(accessibility_str(accessibility));
                    self.raw(" ");
                }
                self.raw("constructor");
                self.emit_params_decl(&constructor.parameters);
                self.raw(";");
                true
            }
            ClassMember::Method(method) => {
                self.emit_member_modifiers_decl(&method.modifiers);
                match method.modifier {
                    PropertyModifier::Get => self.raw("get "),
                    PropertyModifier::Set => self.raw("set "),
                    PropertyModifier::None => {}
                }
                self.emit_property_name(&method.name);
                if method.optional {
                    self.raw("?");
                }
                self.emit_type_parameters(&method.function.type_parameters);
                self.emit_params_decl(&method.function.parameters);
                if let Some(return_type) = &method.function.return_type {
                    self.raw(": ");
                    self.emit_type(&return_type.data().type_node);
                }
                self.raw(";");
                true
            }
            ClassMember::Property(property) => {
                self.emit_member_modifiers_decl(&property.modifiers);
                self.emit_property_name(&property.name);
                if property.optional {
                    self.raw("?");
                }
                if let Some(annotation) = &property.type_annotation {
                    self.raw(": ");
                    self.emit_type(&annotation.data().type_node);
                }
                self.raw(";");
                true
            }
            ClassMember::AutoAccessor(accessor) => {
                self.emit_member_modifiers_decl(&accessor.modifiers);
                self.raw("accessor ");
                self.emit_property_name(&accessor.name);
                if let Some(annotation) = &accessor.type_annotation {
                    self.raw(": ");
                    self.emit_type(&annotation.data().type_node);
                }
                self.raw(";");
                true
            }
            ClassMember::IndexSignature(signature) => {
                if signature.readonly {
                    self.raw("readonly ");
                }
                self.raw("[");
                self.emit_params_decl_inner(&signature.parameters);
                self.raw("]: ");
                self.emit_type(&signature.type_annotation.data().type_node);
                self.raw(";");
                true
            }
            ClassMember::StaticBlock(_) => false,
            ClassMember::Missing(_) => {
                self.diag_here(codes::MISSING_MEMBER, "cannot emit a missing class member");
                false
            }
        }
    }

    fn emit_params_decl_inner(&mut self, parameters: &[ParameterNode]) {
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            let parameter = parameter.data();
            self.emit_pattern(&parameter.binding);
            if let Some(annotation) = &parameter.type_annotation {
                self.raw(": ");
                self.emit_type(&annotation.data().type_node);
            }
        }
    }

    fn emit_member_modifiers_decl(&mut self, modifiers: &DeclarationModifiers) {
        if let Some(accessibility) = modifiers.accessibility {
            self.raw(accessibility_str(accessibility));
            self.raw(" ");
        }
        if modifiers.is_static {
            self.raw("static ");
        }
        if modifiers.is_abstract {
            self.raw("abstract ");
        }
        if modifiers.is_readonly {
            self.raw("readonly ");
        }
    }

    fn emit_interface_decl(&mut self, interface: &InterfaceDeclaration) {
        self.raw("interface ");
        self.emit_ident(&interface.name);
        self.emit_type_parameters(&interface.type_parameters);
        if !interface.extends.is_empty() {
            self.raw(" extends ");
            for (index, reference) in interface.extends.iter().enumerate() {
                if index > 0 {
                    self.raw(", ");
                }
                self.emit_type_reference(reference);
            }
        }
        self.raw(" ");
        self.emit_type_members_block(&interface.members);
    }

    fn emit_type_alias_decl(&mut self, alias: &TypeAliasDeclaration) {
        self.raw("type ");
        self.emit_ident(&alias.name);
        self.emit_type_parameters(&alias.type_parameters);
        self.raw(" = ");
        self.emit_type(&alias.type_node);
        self.raw(";");
    }

    fn emit_enum_decl(&mut self, declaration: &EnumDeclaration) {
        self.emit_declare_prefix();
        if declaration.is_const {
            self.raw("const ");
        }
        self.raw("enum ");
        self.emit_ident(&declaration.name);
        self.raw(" ");
        if declaration.members.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        let count = declaration.members.len();
        for (index, member) in declaration.members.iter().enumerate() {
            let member = member.data();
            self.emit_property_name(&member.name);
            if let Some(initializer) = &member.initializer {
                self.raw(" = ");
                self.emit_expression_prec(initializer, P_ASSIGN);
            }
            if index + 1 < count {
                self.raw(",");
            }
            self.newline();
        }
        self.indent -= 1;
        self.raw("}");
    }

    fn emit_namespace_decl(&mut self, namespace: &NamespaceDeclaration) {
        self.emit_declare_prefix();
        match &namespace.name {
            NamespaceName::Identifier { name, keyword } => {
                self.raw(keyword.as_str());
                self.raw(" ");
                self.emit_ident(name);
            }
            NamespaceName::StringLiteral(literal) => {
                self.raw("module ");
                self.emit_string(literal);
            }
            NamespaceName::Global { .. } => {
                self.raw("global");
            }
        }
        self.raw(" {");
        let body = namespace.body.data();
        if body.statements.is_empty() {
            self.raw("}");
            return;
        }
        self.newline();
        self.indent += 1;
        let saved_ambient = self.decl_ambient;
        self.decl_ambient = false;
        for statement in &body.statements {
            if self.emit_declaration(statement) {
                self.newline();
            }
        }
        self.decl_ambient = saved_ambient;
        self.indent -= 1;
        self.raw("}");
    }

    // ---- types ------------------------------------------------------------

    fn emit_type_parameters(&mut self, parameters: &Option<TypeParameterList>) {
        let Some(list) = parameters else {
            return;
        };
        if list.parameters.is_empty() {
            return;
        }
        self.raw("<");
        for (index, parameter) in list.parameters.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            self.emit_type_parameter(parameter.data());
        }
        self.raw(">");
    }

    fn emit_type_parameter(&mut self, parameter: &TypeParameter) {
        match parameter.variance {
            Variance::In => self.raw("in "),
            Variance::Out => self.raw("out "),
            Variance::InOut => self.raw("in out "),
            Variance::Invariant => {}
        }
        self.emit_ident(&parameter.name);
        if let Some(constraint) = &parameter.constraint {
            self.raw(" extends ");
            self.emit_type(constraint);
        }
        if let Some(default) = &parameter.default {
            self.raw(" = ");
            self.emit_type(default);
        }
    }

    fn emit_type_arguments(&mut self, arguments: &Option<TypeArgumentList>) {
        let Some(list) = arguments else {
            return;
        };
        if list.arguments.is_empty() {
            return;
        }
        self.raw("<");
        for (index, argument) in list.arguments.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            self.emit_type(argument);
        }
        self.raw(">");
    }

    fn emit_type_reference(&mut self, reference: &TypeReference) {
        self.emit_entity_name(&reference.name);
        self.emit_type_arguments(&reference.type_arguments);
    }

    fn emit_type(&mut self, ty: &Ty) {
        let previous = self.anchor;
        self.anchor = ty.range();
        match ty.data() {
            TypeNode::Keyword(keyword) => self.raw(keyword_type_str(*keyword)),
            TypeNode::Literal(literal) => self.emit_type_literal(literal),
            TypeNode::Reference(reference) => self.emit_type_reference(reference),
            TypeNode::Union(members) => self.emit_union(members),
            TypeNode::Intersection(members) => self.emit_intersection(members),
            TypeNode::Array(element) => {
                self.emit_type_postfix_operand(element);
                self.raw("[]");
            }
            TypeNode::Tuple(tuple) => self.emit_tuple(tuple),
            TypeNode::Object(object) => self.emit_object_type(object),
            TypeNode::Function(function) => self.emit_function_type(function),
            TypeNode::Constructor(constructor) => {
                if constructor.is_abstract {
                    self.raw("abstract ");
                }
                self.raw("new ");
                self.emit_function_type_body(&constructor.function);
            }
            TypeNode::Query(query) => {
                self.raw("typeof ");
                self.emit_entity_name(&query.name);
                self.emit_type_arguments(&query.type_arguments);
            }
            TypeNode::Operator { operator, operand } => {
                self.raw(type_operator_str(*operator));
                self.raw(" ");
                self.emit_type_operator_operand(operand);
            }
            TypeNode::IndexedAccess(indexed) => {
                self.emit_type_postfix_operand(&indexed.object_type);
                self.raw("[");
                self.emit_type(&indexed.index_type);
                self.raw("]");
            }
            TypeNode::Conditional(conditional) => {
                self.emit_type(&conditional.check_type);
                self.raw(" extends ");
                self.emit_type(&conditional.extends_type);
                self.raw(" ? ");
                self.emit_type(&conditional.true_type);
                self.raw(" : ");
                self.emit_type(&conditional.false_type);
            }
            TypeNode::Mapped(mapped) => self.emit_mapped_type(mapped),
            TypeNode::Infer(infer) => {
                self.raw("infer ");
                self.emit_ident(&infer.parameter.data().name);
                if let Some(constraint) = &infer.parameter.data().constraint {
                    self.raw(" extends ");
                    self.emit_type(constraint);
                }
            }
            TypeNode::Import(import) => {
                self.raw("import(");
                self.emit_string(&import.argument);
                self.raw(")");
                if let Some(qualifier) = &import.qualifier {
                    self.raw(".");
                    self.emit_entity_name(qualifier);
                }
                self.emit_type_arguments(&import.type_arguments);
            }
            TypeNode::TemplateLiteral(template) => self.emit_template_literal_type(template),
            TypeNode::Parenthesized(inner) => self.emit_type(inner),
            TypeNode::This => self.raw("this"),
            TypeNode::Predicate(predicate) => self.emit_type_predicate(predicate),
            TypeNode::Missing(_) => {
                self.diag_here(codes::MISSING_TYPE, "cannot emit a missing type node");
                self.raw("any");
            }
        }
        self.anchor = previous;
    }

    fn emit_type_literal(&mut self, literal: &TypeLiteral) {
        match literal {
            TypeLiteral::String(node) => self.emit_token(node.data().token()),
            TypeLiteral::Number(node) => self.emit_token(node.data().token()),
            TypeLiteral::BigInt(node) => self.emit_token(node.data().token()),
            TypeLiteral::Boolean(node) => self.emit_token(node.data().token()),
            TypeLiteral::Null(node) => match self.text(node.data().token()) {
                Some(text) if !text.is_empty() => self.raw(text),
                _ => self.raw("null"),
            },
            TypeLiteral::Unary { operator, operand } => {
                self.raw(unary_operator_str(*operator));
                self.emit_type(operand);
            }
        }
    }

    fn emit_union(&mut self, members: &[Ty]) {
        for (index, member) in members.iter().enumerate() {
            if index > 0 {
                self.raw(" | ");
            }
            if is_low_precedence_type(self.unwrap_type(member)) {
                self.raw("(");
                self.emit_type(member);
                self.raw(")");
            } else {
                self.emit_type(member);
            }
        }
    }

    fn emit_intersection(&mut self, members: &[Ty]) {
        for (index, member) in members.iter().enumerate() {
            if index > 0 {
                self.raw(" & ");
            }
            let inner = self.unwrap_type(member);
            if is_low_precedence_type(inner) || matches!(inner.data(), TypeNode::Union(_)) {
                self.raw("(");
                self.emit_type(member);
                self.raw(")");
            } else {
                self.emit_type(member);
            }
        }
    }

    fn emit_type_postfix_operand(&mut self, ty: &Ty) {
        let inner = self.unwrap_type(ty);
        let wrap = is_low_precedence_type(inner)
            || matches!(
                inner.data(),
                TypeNode::Union(_) | TypeNode::Intersection(_) | TypeNode::Operator { .. }
            );
        if wrap {
            self.raw("(");
            self.emit_type(ty);
            self.raw(")");
        } else {
            self.emit_type(ty);
        }
    }

    fn emit_type_operator_operand(&mut self, ty: &Ty) {
        let inner = self.unwrap_type(ty);
        if is_low_precedence_type(inner)
            || matches!(inner.data(), TypeNode::Union(_) | TypeNode::Intersection(_))
        {
            self.raw("(");
            self.emit_type(ty);
            self.raw(")");
        } else {
            self.emit_type(ty);
        }
    }

    fn emit_tuple(&mut self, tuple: &TupleType) {
        if tuple.readonly {
            self.raw("readonly ");
        }
        self.raw("[");
        for (index, element) in tuple.elements.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            if element.rest {
                self.raw("...");
            }
            if let Some(name) = &element.name {
                self.emit_ident(name);
                if element.optional {
                    self.raw("?");
                }
                self.raw(": ");
                self.emit_type(&element.type_node);
            } else {
                self.emit_type(&element.type_node);
                if element.optional {
                    self.raw("?");
                }
            }
        }
        self.raw("]");
    }

    fn emit_object_type(&mut self, object: &ObjectType) {
        if object.members.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{ ");
        for (index, member) in object.members.iter().enumerate() {
            if index > 0 {
                self.raw(" ");
            }
            self.emit_type_member(member.data());
        }
        self.raw(" }");
    }

    fn emit_type_members_block(&mut self, members: &[TypeMemberNode]) {
        if members.is_empty() {
            self.raw("{}");
            return;
        }
        self.raw("{");
        self.newline();
        self.indent += 1;
        for member in members {
            self.emit_type_member(member.data());
            self.newline();
        }
        self.indent -= 1;
        self.raw("}");
    }

    fn emit_type_member(&mut self, member: &TypeMember) {
        match member {
            TypeMember::Property(property) => {
                if property.readonly {
                    self.raw("readonly ");
                }
                self.emit_property_name(&property.name);
                if property.optional {
                    self.raw("?");
                }
                if let Some(annotation) = &property.type_annotation {
                    self.raw(": ");
                    self.emit_type(&annotation.data().type_node);
                }
                self.raw(";");
            }
            TypeMember::Method(method) => {
                self.emit_property_name(&method.name);
                if method.optional {
                    self.raw("?");
                }
                self.emit_function_type_body(&method.function);
                self.raw(";");
            }
            TypeMember::Call(call) => {
                self.emit_function_type_body(&call.function);
                self.raw(";");
            }
            TypeMember::Construct(construct) => {
                self.raw("new ");
                self.emit_function_type_body(&construct.function.function);
                self.raw(";");
            }
            TypeMember::Index(index) => {
                if index.readonly {
                    self.raw("readonly ");
                }
                self.raw("[");
                self.emit_function_type_parameters(&index.parameters);
                self.raw("]: ");
                self.emit_type(&index.type_annotation.data().type_node);
                self.raw(";");
            }
            TypeMember::Missing(_) => {
                self.diag_here(codes::MISSING_MEMBER, "cannot emit a missing type member");
            }
        }
    }

    fn emit_function_type(&mut self, function: &FunctionType) {
        self.emit_function_type_body_arrow(function);
    }

    fn emit_function_type_body_arrow(&mut self, function: &FunctionType) {
        self.emit_type_parameters(&function.type_parameters);
        self.raw("(");
        self.emit_function_type_parameters(&function.parameters);
        self.raw(") => ");
        self.emit_type(&function.return_type);
    }

    fn emit_function_type_body(&mut self, function: &FunctionType) {
        self.emit_type_parameters(&function.type_parameters);
        self.raw("(");
        self.emit_function_type_parameters(&function.parameters);
        self.raw("): ");
        self.emit_type(&function.return_type);
    }

    fn emit_function_type_parameters(&mut self, parameters: &[FunctionTypeParameter]) {
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                self.raw(", ");
            }
            if parameter.rest {
                self.raw("...");
            }
            self.emit_ident(&parameter.name);
            if parameter.optional {
                self.raw("?");
            }
            self.raw(": ");
            self.emit_type(&parameter.type_annotation.data().type_node);
        }
    }

    fn emit_mapped_type(&mut self, mapped: &MappedType) {
        self.raw("{ ");
        match mapped.readonly_modifier {
            MappedModifier::Preserve => {}
            MappedModifier::Add => self.raw("readonly "),
            MappedModifier::Remove => self.raw("-readonly "),
        }
        self.raw("[");
        self.emit_ident(&mapped.parameter.data().name);
        self.raw(" in ");
        if let Some(constraint) = &mapped.parameter.data().constraint {
            self.emit_type(constraint);
        }
        if let Some(name_type) = &mapped.name_type {
            self.raw(" as ");
            self.emit_type(name_type);
        }
        self.raw("]");
        match mapped.optional_modifier {
            MappedModifier::Preserve => {}
            MappedModifier::Add => self.raw("?"),
            MappedModifier::Remove => self.raw("-?"),
        }
        if let Some(value_type) = &mapped.value_type {
            self.raw(": ");
            self.emit_type(value_type);
        }
        self.raw("; }");
    }

    fn emit_template_literal_type(&mut self, template: &TemplateLiteralType) {
        if template.elements.is_empty() {
            return;
        }
        self.emit_token(template.elements[0].data().token());
        for (index, ty) in template.types.iter().enumerate() {
            self.emit_type(ty);
            if let Some(element) = template.elements.get(index + 1) {
                self.emit_token(element.data().token());
            }
        }
    }

    fn emit_type_predicate(&mut self, predicate: &TypePredicate) {
        if predicate.asserts {
            self.raw("asserts ");
        }
        self.emit_entity_name(&predicate.parameter_name);
        if let Some(ty) = &predicate.type_node {
            self.raw(" is ");
            self.emit_type(ty);
        }
    }

    fn unwrap_type<'t>(&self, ty: &'t Ty) -> &'t Ty {
        match ty.data() {
            TypeNode::Parenthesized(inner) => self.unwrap_type(inner),
            _ => ty,
        }
    }
}

// ---- free helpers ---------------------------------------------------------

fn is_parameter_property(parameter: &Parameter) -> bool {
    parameter.modifiers.accessibility.is_some()
        || parameter.modifiers.is_readonly
        || parameter.modifiers.is_override
}

const fn variable_kind_str(kind: VariableKind) -> &'static str {
    match kind {
        VariableKind::Var => "var",
        VariableKind::Let => "let",
        VariableKind::Const => "const",
        VariableKind::Using => "using",
        VariableKind::AwaitUsing => "await using",
    }
}

const fn accessibility_str(accessibility: Accessibility) -> &'static str {
    match accessibility {
        Accessibility::Public => "public",
        Accessibility::Protected => "protected",
        Accessibility::Private => "private",
    }
}

const fn binary_prec(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::BitOr => P_BIT_OR,
        BinaryOperator::BitXor => P_BIT_XOR,
        BinaryOperator::BitAnd => P_BIT_AND,
        BinaryOperator::Equal
        | BinaryOperator::NotEqual
        | BinaryOperator::StrictEqual
        | BinaryOperator::StrictNotEqual => P_EQUALITY,
        BinaryOperator::LessThan
        | BinaryOperator::LessThanOrEqual
        | BinaryOperator::GreaterThan
        | BinaryOperator::GreaterThanOrEqual
        | BinaryOperator::In
        | BinaryOperator::Instanceof => P_RELATIONAL,
        BinaryOperator::LeftShift
        | BinaryOperator::SignedRightShift
        | BinaryOperator::UnsignedRightShift => P_SHIFT,
        BinaryOperator::Add | BinaryOperator::Subtract => P_ADDITIVE,
        BinaryOperator::Multiply | BinaryOperator::Divide | BinaryOperator::Remainder => {
            P_MULTIPLICATIVE
        }
        BinaryOperator::Exponentiate => P_EXPONENT,
    }
}

const fn binary_str(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Remainder => "%",
        BinaryOperator::Exponentiate => "**",
        BinaryOperator::LeftShift => "<<",
        BinaryOperator::SignedRightShift => ">>",
        BinaryOperator::UnsignedRightShift => ">>>",
        BinaryOperator::LessThan => "<",
        BinaryOperator::LessThanOrEqual => "<=",
        BinaryOperator::GreaterThan => ">",
        BinaryOperator::GreaterThanOrEqual => ">=",
        BinaryOperator::In => "in",
        BinaryOperator::Instanceof => "instanceof",
        BinaryOperator::Equal => "==",
        BinaryOperator::NotEqual => "!=",
        BinaryOperator::StrictEqual => "===",
        BinaryOperator::StrictNotEqual => "!==",
        BinaryOperator::BitAnd => "&",
        BinaryOperator::BitXor => "^",
        BinaryOperator::BitOr => "|",
    }
}

const fn logical_str(operator: LogicalOperator) -> &'static str {
    match operator {
        LogicalOperator::And => "&&",
        LogicalOperator::Or => "||",
        LogicalOperator::Nullish => "??",
    }
}

const fn assignment_str(operator: AssignmentOperator) -> &'static str {
    match operator {
        AssignmentOperator::Assign => "=",
        AssignmentOperator::AddAssign => "+=",
        AssignmentOperator::SubtractAssign => "-=",
        AssignmentOperator::MultiplyAssign => "*=",
        AssignmentOperator::DivideAssign => "/=",
        AssignmentOperator::RemainderAssign => "%=",
        AssignmentOperator::ExponentiateAssign => "**=",
        AssignmentOperator::LeftShiftAssign => "<<=",
        AssignmentOperator::SignedRightShiftAssign => ">>=",
        AssignmentOperator::UnsignedRightShiftAssign => ">>>=",
        AssignmentOperator::BitAndAssign => "&=",
        AssignmentOperator::BitXorAssign => "^=",
        AssignmentOperator::BitOrAssign => "|=",
        AssignmentOperator::LogicalAndAssign => "&&=",
        AssignmentOperator::LogicalOrAssign => "||=",
        AssignmentOperator::NullishAssign => "??=",
    }
}

const fn unary_operator_str(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Plus => "+",
        UnaryOperator::Minus => "-",
        UnaryOperator::Not => "!",
        UnaryOperator::BitNot => "~",
        UnaryOperator::Typeof => "typeof ",
        UnaryOperator::Void => "void ",
        UnaryOperator::Delete => "delete ",
    }
}

const fn keyword_type_str(keyword: KeywordType) -> &'static str {
    match keyword {
        KeywordType::Any => "any",
        KeywordType::Unknown => "unknown",
        KeywordType::Never => "never",
        KeywordType::Void => "void",
        KeywordType::Undefined => "undefined",
        KeywordType::Null => "null",
        KeywordType::Boolean => "boolean",
        KeywordType::Number => "number",
        KeywordType::BigInt => "bigint",
        KeywordType::String => "string",
        KeywordType::Symbol => "symbol",
        KeywordType::Object => "object",
        KeywordType::Intrinsic => "intrinsic",
    }
}

const fn type_operator_str(operator: TypeOperator) -> &'static str {
    match operator {
        TypeOperator::Keyof => "keyof",
        TypeOperator::Unique => "unique",
        TypeOperator::Readonly => "readonly",
    }
}

fn is_low_precedence_type(ty: &Ty) -> bool {
    matches!(
        ty.data(),
        TypeNode::Function(_)
            | TypeNode::Constructor(_)
            | TypeNode::Conditional(_)
            | TypeNode::Infer(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ScriptKind, SourceId, SourceText, Utf16Pos};
    use std::sync::Arc;

    /// Builds an AST with token ranges that point into a real source string, so
    /// the emitter resolves identifier and literal lexemes exactly as it would
    /// for a parser product.
    struct Builder {
        text: String,
        len: usize,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                text: String::new(),
                len: 0,
            }
        }

        fn range(start: usize, end: usize) -> TextRange {
            TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered range")
        }

        fn token(&mut self, kind: TokenKind, lexeme: &str) -> Token {
            let start = self.len;
            self.text.push_str(lexeme);
            self.len += lexeme.encode_utf16().count();
            Token::new(kind, Self::range(start, self.len))
        }

        fn ident(&mut self, name: &str) -> IdentifierNode {
            let token = self.token(TokenKind::Identifier, name);
            Node::new(NodeId::new(0), token.range(), Identifier::new(token))
        }

        fn ident_expr(&mut self, name: &str) -> Expr {
            let ident = self.ident(name);
            let range = ident.range();
            Node::new(NodeId::new(0), range, Expression::Identifier(ident))
        }

        fn number(&mut self, literal: &str) -> Expr {
            let token = self.token(TokenKind::NumericLiteral, literal);
            let node = Node::new(NodeId::new(0), token.range(), NumericLiteral::new(token));
            let range = node.range();
            Node::new(
                NodeId::new(0),
                range,
                Expression::Literal(Literal::Number(node)),
            )
        }

        fn string(&mut self, literal: &str) -> StringLiteralNode {
            let token = self.token(TokenKind::StringLiteral, literal);
            Node::new(NodeId::new(0), token.range(), StringLiteral::new(token))
        }

        fn finish(self, statements: Vec<Stmt>) -> SourceFile {
            let source =
                Arc::new(SourceText::new(self.text).expect("test source fits the per-file budget"));
            let len = source.len_utf16();
            let full = TextRange::new(Utf16Pos::ZERO, len).expect("range");
            let eof = Token::new(
                TokenKind::EndOfFile,
                TextRange::new(len, len).expect("range"),
            );
            SourceFile::new(
                NodeId::new(0),
                SourceId::new(0),
                ScriptKind::TypeScript,
                full,
                source,
                Vec::new(),
                statements,
                eof,
                Vec::new(),
            )
        }
    }

    fn dummy() -> TextRange {
        TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO).expect("range")
    }

    fn expr(data: Expression) -> Expr {
        Node::new(NodeId::new(0), dummy(), data)
    }

    fn stmt(data: Statement) -> Stmt {
        Node::new(NodeId::new(0), dummy(), data)
    }

    fn expr_stmt(expression: Expr) -> Stmt {
        stmt(Statement::Expression(ExpressionStatement {
            expression: Box::new(expression),
        }))
    }

    fn binary(operator: BinaryOperator, left: Expr, right: Expr) -> Expr {
        expr(Expression::Binary(BinaryExpression {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        }))
    }

    fn emit_js(file: &SourceFile) -> EmitOutput {
        emit(file, EmitOptions::javascript())
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        // a + b * c -- no parentheses required.
        let tree = binary(
            BinaryOperator::Add,
            a,
            binary(BinaryOperator::Multiply, bb, c),
        );
        let file = b.finish(vec![expr_stmt(tree)]);
        assert_eq!(emit_js(&file).code, "a + b * c;\n");
    }

    #[test]
    fn lower_precedence_left_operand_is_parenthesized() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        // (a + b) * c -- the additive left operand needs parentheses.
        let tree = binary(
            BinaryOperator::Multiply,
            binary(BinaryOperator::Add, a, bb),
            c,
        );
        let file = b.finish(vec![expr_stmt(tree)]);
        assert_eq!(emit_js(&file).code, "(a + b) * c;\n");
    }

    #[test]
    fn exponent_is_right_associative_and_forces_left_grouping() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        // (a ** b) ** c must keep the left grouping; a ** b ** c must not add any.
        let left_grouped = binary(
            BinaryOperator::Exponentiate,
            binary(BinaryOperator::Exponentiate, a, bb),
            c,
        );
        let file = b.finish(vec![expr_stmt(left_grouped)]);
        assert_eq!(emit_js(&file).code, "(a ** b) ** c;\n");
    }

    #[test]
    fn unary_left_of_exponent_is_parenthesized() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let neg = expr(Expression::Unary(UnaryExpression {
            operator: UnaryOperator::Minus,
            argument: Box::new(a),
        }));
        let tree = binary(BinaryOperator::Exponentiate, neg, bb);
        let file = b.finish(vec![expr_stmt(tree)]);
        assert_eq!(emit_js(&file).code, "(-a) ** b;\n");
    }

    #[test]
    fn nullish_and_logical_or_require_parentheses_when_mixed() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        // (a || b) ?? c -- mixing ?? with || is a syntax error without parens.
        let or = expr(Expression::Logical(LogicalExpression {
            operator: LogicalOperator::Or,
            left: Box::new(a),
            right: Box::new(bb),
        }));
        let tree = expr(Expression::Logical(LogicalExpression {
            operator: LogicalOperator::Nullish,
            left: Box::new(or),
            right: Box::new(c),
        }));
        let file = b.finish(vec![expr_stmt(tree)]);
        assert_eq!(emit_js(&file).code, "(a || b) ?? c;\n");
    }

    #[test]
    fn object_literal_expression_statement_is_parenthesized() {
        let b = Builder::new();
        let object = expr(Expression::Object(ObjectLiteral {
            members: Vec::new(),
        }));
        let file = b.finish(vec![expr_stmt(object)]);
        assert_eq!(emit_js(&file).code, "({});\n");
    }

    #[test]
    fn arrow_object_body_is_parenthesized() {
        let b = Builder::new();
        let object = expr(Expression::Object(ObjectLiteral {
            members: Vec::new(),
        }));
        let arrow = expr(Expression::Arrow(ArrowFunction {
            is_async: false,
            type_parameters: None,
            parameters: Vec::new(),
            return_type: None,
            body: FunctionBody::Expression(Box::new(object)),
        }));
        let file = b.finish(vec![expr_stmt(arrow)]);
        assert_eq!(emit_js(&file).code, "() => ({});\n");
    }

    #[test]
    fn type_annotation_is_erased_in_javascript() {
        let mut b = Builder::new();
        let name = b.ident("x");
        let init = b.number("1");
        let annotation = Node::new(
            NodeId::new(0),
            dummy(),
            TypeAnnotation {
                type_node: Box::new(Node::new(
                    NodeId::new(0),
                    dummy(),
                    TypeNode::Keyword(KeywordType::Number),
                )),
            },
        );
        let binding = Node::new(
            NodeId::new(0),
            name.range(),
            BindingPattern::Identifier(name),
        );
        let declarator = Node::new(
            NodeId::new(0),
            dummy(),
            VariableDeclarator {
                binding,
                definite: false,
                type_annotation: Some(annotation),
                initializer: Some(Box::new(init)),
            },
        );
        let declaration = stmt(Statement::Variable(VariableDeclaration {
            range: dummy(),
            kind: VariableKind::Let,
            declarations: vec![declarator],
        }));
        let file = b.finish(vec![declaration]);
        assert_eq!(emit_js(&file).code, "let x = 1;\n");
    }

    #[test]
    fn as_expression_is_erased_but_keeps_needed_parentheses() {
        let mut b = Builder::new();
        let a = b.ident_expr("a");
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        // (a + b as T) * c erases to (a + b) * c.
        let as_expr = expr(Expression::As(AsExpression {
            expression: Box::new(binary(BinaryOperator::Add, a, bb)),
            type_node: Some(Box::new(Node::new(
                NodeId::new(0),
                dummy(),
                TypeNode::Keyword(KeywordType::Any),
            ))),
        }));
        let tree = binary(BinaryOperator::Multiply, as_expr, c);
        let file = b.finish(vec![expr_stmt(tree)]);
        assert_eq!(emit_js(&file).code, "(a + b) * c;\n");
    }

    #[test]
    fn newline_policy_uses_configured_terminator() {
        let mut b = Builder::new();
        let first = expr_stmt(b.ident_expr("a"));
        let second = expr_stmt(b.ident_expr("b"));
        let file = b.finish(vec![first, second]);
        let output = emit(&file, EmitOptions::javascript().with_newline(Newline::CrLf));
        assert_eq!(output.code, "a;\r\nb;\r\n");
    }

    #[test]
    fn const_enum_scalar_use_keeps_block_indentation() {
        // A const-enum member used as a bare expression statement inside an
        // indented block is inlined to its numeric scalar. That scalar must
        // flow through the indentation machinery (`raw`) like every other
        // emitted token, so the line keeps its leading spaces.
        let source = Arc::new(
            SourceText::new("const enum K { X = 2 }\n{ K.X; }")
                .expect("test source fits the per-file budget"),
        );
        let scanned = crate::scanner::scan(SourceId::new(0), ScriptKind::TypeScript, source);
        let parsed = crate::parser::parse(scanned);
        let output = emit(parsed.product(), EmitOptions::javascript());
        // The const enum declaration is erased; only the inlined block remains.
        assert_eq!(output.code, "{\n    2;\n}\n");
    }

    #[test]
    fn missing_expression_reports_stable_diagnostic() {
        let b = Builder::new();
        let missing = expr(Expression::Missing(MissingNode::new(
            NodeKind::IdentifierExpression,
        )));
        let file = b.finish(vec![expr_stmt(missing)]);
        let output = emit_js(&file);
        assert_eq!(output.code, "void 0;\n");
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code(), codes::MISSING_EXPRESSION);
        assert!(output.has_errors());
    }

    #[test]
    fn enum_lowers_to_runtime_object_with_reverse_mapping() {
        let mut b = Builder::new();
        let enum_name = b.ident("E");
        let member_a = Node::new(
            NodeId::new(1),
            dummy(),
            EnumMember {
                name: PropertyName::Identifier(b.ident("A")),
                initializer: None,
            },
        );
        let member_b = Node::new(
            NodeId::new(2),
            dummy(),
            EnumMember {
                name: PropertyName::Identifier(b.ident("B")),
                initializer: None,
            },
        );
        let declaration = Node::new(
            NodeId::new(3),
            dummy(),
            Statement::Enum(EnumDeclaration {
                is_const: false,
                name: enum_name,
                members: vec![member_a, member_b],
            }),
        );
        let file = b.finish(vec![declaration]);
        let expected = "var E;\n(function (E) {\n    E[E[\"A\"] = 0] = \"A\";\n    E[E[\"B\"] = 1] = \"B\";\n})(E || (E = {}));\n";
        assert_eq!(emit_js(&file).code, expected);
    }

    #[test]
    fn enum_string_member_has_no_reverse_mapping() {
        let mut b = Builder::new();
        let enum_name = b.ident("E");
        let value = b.string("\"hi\"");
        let member = Node::new(
            NodeId::new(1),
            dummy(),
            EnumMember {
                name: PropertyName::Identifier(b.ident("A")),
                initializer: Some(Box::new(expr(Expression::Literal(Literal::String(value))))),
            },
        );
        let declaration = Node::new(
            NodeId::new(2),
            dummy(),
            Statement::Enum(EnumDeclaration {
                is_const: false,
                name: enum_name,
                members: vec![member],
            }),
        );
        let file = b.finish(vec![declaration]);
        let expected = "var E;\n(function (E) {\n    E[\"A\"] = \"hi\";\n})(E || (E = {}));\n";
        assert_eq!(emit_js(&file).code, expected);
    }

    #[test]
    fn namespace_reports_unlowered_diagnostic_in_javascript() {
        let mut b = Builder::new();
        let name = b.ident("N");
        let body = Node::new(
            NodeId::new(0),
            dummy(),
            Block {
                statements: Vec::new(),
            },
        );
        let declaration = stmt(Statement::Namespace(NamespaceDeclaration {
            name: NamespaceName::Identifier {
                name,
                keyword: NamespaceKeyword::Namespace,
            },
            body,
        }));
        let file = b.finish(vec![declaration]);
        let output = emit_js(&file);
        assert_eq!(output.code, "");
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code(), codes::NAMESPACE_UNLOWERED);
    }

    #[test]
    fn parameter_property_is_lowered_to_constructor_assignment() {
        let mut b = Builder::new();
        let class_name = b.ident("C");
        let param_name = b.ident("x");
        let binding = Node::new(
            NodeId::new(0),
            param_name.range(),
            BindingPattern::Identifier(param_name),
        );
        let parameter = Node::new(
            NodeId::new(0),
            dummy(),
            Parameter {
                decorators: Vec::new(),
                modifiers: ParameterModifiers {
                    accessibility: Some(Accessibility::Private),
                    is_readonly: false,
                    is_override: false,
                },
                binding,
                optional: false,
                type_annotation: None,
                initializer: None,
            },
        );
        let constructor = Node::new(
            NodeId::new(0),
            dummy(),
            ClassMember::Constructor(ConstructorDeclaration {
                decorators: Vec::new(),
                modifiers: DeclarationModifiers::default(),
                parameters: vec![parameter],
                body: Node::new(
                    NodeId::new(0),
                    dummy(),
                    Block {
                        statements: Vec::new(),
                    },
                ),
            }),
        );
        let class = stmt(Statement::Class(ClassDeclaration {
            decorators: Vec::new(),
            modifiers: DeclarationModifiers::default(),
            name: Some(class_name),
            type_parameters: None,
            extends: None,
            implements: Vec::new(),
            members: vec![constructor],
        }));
        let file = b.finish(vec![class]);
        let expected = "class C {\n    constructor(x) {\n        this.x = x;\n    }\n}\n";
        assert_eq!(emit_js(&file).code, expected);
    }

    #[test]
    fn declaration_mode_prints_type_alias_and_erases_bodies() {
        let mut b = Builder::new();
        let alias_name = b.ident("Id");
        let alias = stmt(Statement::TypeAlias(TypeAliasDeclaration {
            name: alias_name,
            type_parameters: None,
            type_node: Box::new(Node::new(
                NodeId::new(0),
                dummy(),
                TypeNode::Keyword(KeywordType::Number),
            )),
        }));
        let file = b.finish(vec![alias]);
        let output = emit(&file, EmitOptions::declaration());
        assert_eq!(output.code, "type Id = number;\n");
    }

    #[test]
    fn declaration_mode_emits_function_signature_without_body() {
        let mut b = Builder::new();
        let function_name = b.ident("f");
        let function = stmt(Statement::Function(FunctionDeclaration {
            function: FunctionLike {
                decorators: Vec::new(),
                name: Some(function_name),
                is_async: false,
                is_generator: false,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: Some(Node::new(
                    NodeId::new(0),
                    dummy(),
                    TypeAnnotation {
                        type_node: Box::new(Node::new(
                            NodeId::new(0),
                            dummy(),
                            TypeNode::Keyword(KeywordType::Void),
                        )),
                    },
                )),
                body: None,
            },
        }));
        let file = b.finish(vec![function]);
        let output = emit(&file, EmitOptions::declaration());
        assert_eq!(output.code, "declare function f(): void;\n");
    }

    #[test]
    fn union_type_parenthesizes_function_members() {
        let mut b = Builder::new();
        let alias_name = b.ident("U");
        let func_type = Node::new(
            NodeId::new(0),
            dummy(),
            TypeNode::Function(FunctionType {
                type_parameters: None,
                parameters: Vec::new(),
                return_type: Box::new(Node::new(
                    NodeId::new(0),
                    dummy(),
                    TypeNode::Keyword(KeywordType::Void),
                )),
                return_type_missing: false,
            }),
        );
        let number = Node::new(
            NodeId::new(0),
            dummy(),
            TypeNode::Keyword(KeywordType::Number),
        );
        let union = Node::new(
            NodeId::new(0),
            dummy(),
            TypeNode::Union(vec![func_type, number]),
        );
        let alias = stmt(Statement::TypeAlias(TypeAliasDeclaration {
            name: alias_name,
            type_parameters: None,
            type_node: Box::new(union),
        }));
        let file = b.finish(vec![alias]);
        let output = emit(&file, EmitOptions::declaration());
        assert_eq!(output.code, "type U = (() => void) | number;\n");
    }

    #[test]
    fn optional_call_and_member_chain_round_trips() {
        let mut b = Builder::new();
        let object = b.ident_expr("a");
        let member = expr(Expression::Member(MemberExpression {
            object: Box::new(object),
            property: MemberProperty::Named(b.ident("b")),
            optional: true,
        }));
        let call = expr(Expression::Call(CallExpression {
            callee: Box::new(member),
            optional: true,
            type_arguments: None,
            arguments: Vec::new(),
        }));
        let file = b.finish(vec![expr_stmt(call)]);
        assert_eq!(emit_js(&file).code, "a?.b?.();\n");
    }

    #[test]
    fn conditional_test_lower_precedence_gets_parentheses() {
        let mut b = Builder::new();
        let bb = b.ident_expr("b");
        let c = b.ident_expr("c");
        let d = b.ident_expr("d");
        // (a = b) ? c : d -- assignment test must be parenthesized.
        let assign = expr(Expression::Assignment(AssignmentExpression {
            operator: AssignmentOperator::Assign,
            left: Node::new(
                NodeId::new(0),
                dummy(),
                AssignmentTarget::Identifier(b.ident("a2")),
            ),
            right: Box::new(bb),
        }));
        let conditional = expr(Expression::Conditional(ConditionalExpression {
            test: Box::new(assign),
            consequent: Box::new(c),
            alternate: Box::new(d),
        }));
        let file = b.finish(vec![expr_stmt(conditional)]);
        assert_eq!(emit_js(&file).code, "(a2 = b) ? c : d;\n");
    }

    #[test]
    fn export_type_reexport_with_source_erased_in_js() {
        let mut b = Builder::new();
        let specifier = Node::new(
            NodeId::new(0),
            dummy(),
            ExportSpecifier {
                mode: ExportSpecifierMode::TypeOnly,
                local: ModuleExportName::Identifier(b.ident("A")),
                exported: ModuleExportName::Identifier(b.ident("A")),
            },
        );
        let export_stmt = stmt(Statement::Export(ExportDeclaration::Named(
            ExportNamedDeclaration::Specifiers {
                type_only: false,
                specifiers: vec![specifier],
                source: Some(b.string("\"./mod\"")),
                attributes: None,
            },
        )));
        let file = b.finish(vec![export_stmt]);
        assert_eq!(emit_js(&file).code, "");
    }

    #[test]
    fn mixed_type_and_value_reexport_retains_value_exports() {
        let mut b = Builder::new();
        let type_spec = Node::new(
            NodeId::new(0),
            dummy(),
            ExportSpecifier {
                mode: ExportSpecifierMode::TypeOnly,
                local: ModuleExportName::Identifier(b.ident("A")),
                exported: ModuleExportName::Identifier(b.ident("A")),
            },
        );
        let val_spec = Node::new(
            NodeId::new(0),
            dummy(),
            ExportSpecifier {
                mode: ExportSpecifierMode::Value,
                local: ModuleExportName::Identifier(b.ident("B")),
                exported: ModuleExportName::Identifier(b.ident("B")),
            },
        );
        let export_stmt = stmt(Statement::Export(ExportDeclaration::Named(
            ExportNamedDeclaration::Specifiers {
                type_only: false,
                specifiers: vec![type_spec, val_spec],
                source: Some(b.string("\"./mod\"")),
                attributes: None,
            },
        )));
        let file = b.finish(vec![export_stmt]);
        assert_eq!(emit_js(&file).code, "export { B } from \"./mod\";\n");
    }

    #[test]
    fn export_assignment_emits_default_export_in_javascript_mode() {
        let input = "const answer = 42;
export = answer;";
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(SourceText::new(input).expect("test source fits the per-file budget")),
        ));
        assert!(parsed.diagnostics().is_empty());

        let output = emit_js(parsed.product());
        assert!(!output.has_errors());
        assert_eq!(
            output.code,
            "const answer = 42;
export default answer;
"
        );

        let reparsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(
                SourceText::new(output.code.as_str())
                    .expect("test source fits the per-file budget"),
            ),
        ));
        assert!(reparsed.diagnostics().is_empty());
    }

    #[test]
    fn decorators_round_trip_through_javascript_emit() {
        let input = "@classFirst @classSecond class C {\n\
                     @constructorFirst @constructorSecond\n\
                     constructor(@constructorParameterFirst @constructorParameterSecond parameter) {}\n\
                     @methodFirst @methodSecond\n\
                     method(@methodParameterFirst @methodParameterSecond parameter) {}\n\
                     @propertyFirst @propertySecond\n\
                     property = 1;\n\
                     @accessorFirst @accessorSecond\n\
                     accessor value = 2;\n\
                     }";
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(SourceText::new(input).expect("test source fits the per-file budget")),
        ));
        assert!(parsed.diagnostics().is_empty());

        let output = emit_js(parsed.product());
        assert!(!output.has_errors());
        assert_eq!(
            output.code,
            concat!(
                "@classFirst\n",
                "@classSecond\n",
                "class C {\n",
                "    @constructorFirst\n",
                "    @constructorSecond\n",
                "    constructor(@constructorParameterFirst @constructorParameterSecond parameter) {}\n",
                "    @methodFirst\n",
                "    @methodSecond\n",
                "    method(@methodParameterFirst @methodParameterSecond parameter) {}\n",
                "    @propertyFirst\n",
                "    @propertySecond\n",
                "    property = 1;\n",
                "    @accessorFirst\n",
                "    @accessorSecond\n",
                "    accessor value = 2;\n",
                "}\n",
            ),
        );

        let reparsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(
                SourceText::new(output.code.as_str())
                    .expect("test source fits the per-file budget"),
            ),
        ));
        assert!(reparsed.diagnostics().is_empty());
        let file = reparsed.product();
        let source = file.source_text();
        let decorator_texts = |decorators: &[DecoratorNode]| {
            decorators
                .iter()
                .map(|decorator| {
                    let range = decorator.range();
                    let start = source
                        .utf16_to_byte(range.start())
                        .expect("decorator range starts on a source boundary");
                    let end = source
                        .utf16_to_byte(range.end())
                        .expect("decorator range ends on a source boundary");
                    &source.as_str()[start..end]
                })
                .collect::<Vec<_>>()
        };

        let Statement::Class(class) = file.statements()[0].data() else {
            panic!("expected a class declaration");
        };
        assert_eq!(
            decorator_texts(&class.decorators),
            ["@classFirst", "@classSecond"]
        );

        let ClassMember::Constructor(constructor) = class.members[0].data() else {
            panic!("expected a constructor");
        };
        assert_eq!(
            decorator_texts(&constructor.decorators),
            ["@constructorFirst", "@constructorSecond"]
        );
        assert_eq!(
            decorator_texts(&constructor.parameters[0].data().decorators),
            ["@constructorParameterFirst", "@constructorParameterSecond"]
        );

        let ClassMember::Method(method) = class.members[1].data() else {
            panic!("expected a method");
        };
        assert_eq!(
            decorator_texts(&method.function.decorators),
            ["@methodFirst", "@methodSecond"]
        );
        assert_eq!(
            decorator_texts(&method.function.parameters[0].data().decorators),
            ["@methodParameterFirst", "@methodParameterSecond"]
        );

        let ClassMember::Property(property) = class.members[2].data() else {
            panic!("expected a property");
        };
        assert_eq!(
            decorator_texts(&property.decorators),
            ["@propertyFirst", "@propertySecond"]
        );

        let ClassMember::AutoAccessor(accessor) = class.members[3].data() else {
            panic!("expected an auto-accessor");
        };
        assert_eq!(
            decorator_texts(&accessor.decorators),
            ["@accessorFirst", "@accessorSecond"]
        );
    }

    #[test]
    fn declaration_mode_emits_ambient_module_forms() {
        let source = Arc::new(SourceText::new(
            "declare module \"pkg\" { export interface X {} }\ndeclare global { interface Window { x: number } }\ndeclare namespace Foo {}\ndeclare module Bar {}",
        ).expect("test source fits the per-file budget"));
        let scanned = crate::scanner::scan(SourceId::new(0), ScriptKind::TypeScript, source);
        let parsed = crate::parser::parse(scanned);
        let output = emit(parsed.product(), EmitOptions::declaration());
        assert!(
            output.code.contains("declare module \"pkg\""),
            "got: {}",
            output.code
        );
        assert!(
            output.code.contains("declare global"),
            "got: {}",
            output.code
        );
        assert!(
            output.code.contains("declare namespace Foo"),
            "got: {}",
            output.code
        );
        assert!(
            output.code.contains("declare module Bar"),
            "identifier-named module must round-trip module keyword, got: {}",
            output.code
        );
        assert!(
            !output.code.contains("declare namespace Bar"),
            "module Bar must not be rewritten to namespace, got: {}",
            output.code
        );
    }

    #[test]
    fn javascript_mode_reports_unlowered_for_string_and_global_namespaces() {
        let source = Arc::new(
            SourceText::new("module \"pkg\" {}").expect("test source fits the per-file budget"),
        );
        let scanned = crate::scanner::scan(SourceId::new(0), ScriptKind::TypeScript, source);
        let parsed = crate::parser::parse(scanned);
        let string_out = emit_js(parsed.product());
        assert_eq!(string_out.code, "");
        assert!(
            string_out
                .diagnostics
                .iter()
                .any(|d| d.code() == codes::NAMESPACE_UNLOWERED)
        );

        let b = Builder::new();
        let body = Node::new(
            NodeId::new(0),
            dummy(),
            Block {
                statements: Vec::new(),
            },
        );
        let declaration = stmt(Statement::Namespace(NamespaceDeclaration {
            name: NamespaceName::Global { range: dummy() },
            body,
        }));
        let global_file = b.finish(vec![declaration]);
        let global_out = emit_js(&global_file);
        assert_eq!(global_out.code, "");
        assert_eq!(global_out.diagnostics.len(), 1);
        assert_eq!(global_out.diagnostics[0].code(), codes::NAMESPACE_UNLOWERED);
    }
}
