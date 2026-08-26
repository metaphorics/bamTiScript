//! Target-directed downlevel transforms over the existing emit AST.
//!
//! Feature detection walks the immutable [`SourceFile`]. When the requested
//! [`ScriptTarget`] does not natively include a feature, the tree is rewritten
//! in place — synthetic identifiers are interned into an extended [`SourceText`]
//! so the core printer can resolve them. Helper demand is recorded by the
//! rewrite itself and emitted through that same printer.
//!
//! # Guarantees
//! * **Deterministic.** Temps are allocated with a monotonic counter and helper
//!   names are interned in a fixed catalog order.
//! * **Byte-stable.** Identical input and derived [`TransformOptions`] produce
//!   identical JavaScript and diagnostic vectors.
//! * **Negative paths.** Private fields, `using`, `for await`, classes below
//!   ES2015, and generators below ES2015 produce typed diagnostics instead of
//!   silently emitting later syntax.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::checker::SemanticModel;
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::jsx_desugar::{
    JsxEmitOptions, JsxRuntimeBinding, JsxRuntimeImportStyle, JsxSourceDesugarPlan,
    desugar_source_jsx,
};
use crate::source::{
    JsxEmit, NodeIdSource, SourceId, SourcePositionError, SourceText, TextRange, Utf16Pos,
};
use crate::syntax::*;

use super::helpers::{self, HelperKind, HelperOptions};
use super::{EmitFileNames, EmitOutput, Newline, PrintOptions, Surface, print_with_jsx_plan};

/// Stable diagnostic identifiers produced by target transforms.
pub mod codes {
    use crate::diagnostic::DiagnosticCode;

    /// `class` syntax below ES2015 cannot be downleveled by this layer.
    pub const CLASS_REQUIRES_ES2015: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1101");
    /// Generator syntax below ES2015 cannot be downleveled by this layer.
    pub const GENERATOR_REQUIRES_ES2015: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1102");
    /// Private fields below ES2022 require helpers this layer does not apply.
    pub const PRIVATE_FIELD_REQUIRES_ES2022: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1103");
    /// `using` / `await using` is not downleveled.
    pub const USING_UNSUPPORTED: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1104");
    /// `for await` below ES2018 is not downleveled.
    pub const ASYNC_ITERATION_REQUIRES_ES2018: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1105");
    /// A static field on an unnamed class expression cannot be lowered.
    pub const STATIC_FIELD_REQUIRES_CLASS_NAME: DiagnosticCode =
        DiagnosticCode::new("TS-EMIT-1106");
    /// Destructuring assignment (not a declaration) below ES2015 is not lowered.
    pub const DESTRUCTURING_ASSIGNMENT_REQUIRES_ES2015: DiagnosticCode =
        DiagnosticCode::new("TS-EMIT-1107");
    /// Target rewriting exceeded the compiler's bounded per-file text budget.
    pub const SOURCE_TOO_LARGE: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1108");
}

/// ECMAScript language target. Later variants include every earlier feature.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ScriptTarget {
    Es3,
    Es5,
    Es2015,
    Es2016,
    Es2017,
    Es2018,
    Es2019,
    Es2020,
    Es2021,
    Es2022,
    Es2023,
    Es2024,
    #[default]
    EsNext,
}

/// A language feature with a first-supporting [`ScriptTarget`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LanguageFeature {
    Classes,
    Generators,
    Destructuring,
    AsyncFunctions,
    ObjectRestSpread,
    AsyncIteration,
    OptionalChaining,
    NullishCoalescing,
    LogicalAssignment,
    ClassFields,
    Using,
}

impl LanguageFeature {
    /// Returns the earliest target that natively includes this feature.
    #[must_use]
    pub const fn since(self) -> ScriptTarget {
        match self {
            Self::Classes | Self::Generators | Self::Destructuring => ScriptTarget::Es2015,
            Self::AsyncFunctions => ScriptTarget::Es2017,
            Self::ObjectRestSpread | Self::AsyncIteration => ScriptTarget::Es2018,
            Self::OptionalChaining | Self::NullishCoalescing => ScriptTarget::Es2020,
            Self::LogicalAssignment => ScriptTarget::Es2021,
            Self::ClassFields => ScriptTarget::Es2022,
            Self::Using => ScriptTarget::EsNext,
        }
    }
}

impl ScriptTarget {
    /// Returns whether this target natively includes `feature`.
    #[must_use]
    pub const fn supports(self, feature: LanguageFeature) -> bool {
        (self as u8) >= (feature.since() as u8)
    }
}

/// Stage-internal transform view derived from the canonical emit options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransformOptions {
    /// Language target that drives downlevel decisions.
    pub target: ScriptTarget,
    /// When true, class fields that survive downlevel use `define` semantics.
    /// Defaults to true for targets that natively include class fields.
    pub use_define_for_class_fields: bool,
    /// Helper import policy applied after rewrite.
    pub helpers: HelperOptions,
    /// Printer newline.
    pub newline: Newline,
    /// Printer indent width.
    pub indent_width: u8,
    /// JSX mode and factories used to build one source-level desugar plan.
    pub jsx: Option<JsxEmit>,
    pub jsx_factory: Option<Arc<str>>,
    pub jsx_fragment_factory: Option<Arc<str>>,
    pub jsx_import_source: Option<Arc<str>>,
    pub(crate) jsx_import_style: JsxRuntimeImportStyle,
    /// Whether to produce an external JavaScript source map.
    pub source_map: bool,
    /// Whether to produce and inline a JavaScript source map.
    pub inline_source_map: bool,
}

/// Features found in a file, helpers they require, and diagnostics they produce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransformPlan {
    /// Features present in the source, in catalog order.
    pub features: Vec<LanguageFeature>,
    /// Features that the target does not natively include.
    pub required: Vec<LanguageFeature>,
    /// Helpers the rewrite will call, in catalog order.
    pub helpers: Vec<HelperKind>,
    /// Diagnostics produced by analysis, in canonical order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Analyzes `file` without rewriting it.
#[must_use]
pub fn analyze(file: &SourceFile, options: &TransformOptions) -> TransformPlan {
    let mut features = BTreeSet::new();
    let mut diagnostics = Vec::new();
    scan_statements(
        file,
        file.statements(),
        options,
        &mut features,
        &mut diagnostics,
        false,
    );
    let required: Vec<LanguageFeature> = features
        .iter()
        .copied()
        .filter(|feature| !options.target.supports(*feature))
        .collect();
    let mut helpers = BTreeSet::new();
    if required.contains(&LanguageFeature::AsyncFunctions) {
        helpers.insert(HelperKind::Awaiter);
        helpers.insert(HelperKind::Generator);
    }
    if required.contains(&LanguageFeature::ObjectRestSpread)
        || required.contains(&LanguageFeature::Destructuring)
    {
        helpers.insert(HelperKind::Rest);
    }
    diagnostics.sort();
    TransformPlan {
        features: features.into_iter().collect(),
        required,
        helpers: helpers.into_iter().collect(),
        diagnostics,
    }
}

/// Rewrites `file` for `options.target` and prints it through the core printer.
#[must_use]
pub fn emit_transformed(
    file: &SourceFile,
    model: &SemanticModel,
    options: &TransformOptions,
    names: &EmitFileNames,
) -> EmitOutput {
    let plan = analyze(file, options);
    let jsx_plan = executable_jsx_plan(file, options, names);
    let mut jsx_diagnostics = Vec::new();
    if let Err(diagnostic) = &jsx_plan {
        jsx_diagnostics.push((**diagnostic).clone());
    }
    let mut jsx_plan = jsx_plan.ok().flatten();
    let runtime_prelude = jsx_plan
        .as_mut()
        .map(|plan| bind_and_render_jsx_runtime(file, plan))
        .unwrap_or_default();

    let mut rewriter = Rewriter::new(file, options);
    if jsx_plan
        .as_ref()
        .is_some_and(|plan| plan.demand.needs_assign)
    {
        rewriter.used_helpers.insert(HelperKind::Assign);
    }
    let statements = rewriter.rewrite_statements(file.statements());
    let used_helpers: Vec<_> = rewriter.used_helpers.iter().copied().collect();
    let (source, eof, range) = match rewriter.finish_source() {
        Ok(parts) => parts,
        Err(_error) => {
            let mut diagnostics = plan.diagnostics;
            diagnostics.extend(jsx_diagnostics);
            diagnostics.extend(rewriter.diagnostics);
            diagnostics.push(Diagnostic::error(
                codes::SOURCE_TOO_LARGE,
                file.source_id(),
                file.range(),
                "transformed source text exceeds the per-file budget",
            ));
            diagnostics.sort();
            diagnostics.dedup();
            return EmitOutput {
                diagnostics,
                ..EmitOutput::default()
            };
        }
    };
    let rewritten = SourceFile::new(
        file.id(),
        file.source_id(),
        file.script_kind(),
        range,
        source,
        file.tokens().to_vec(),
        statements,
        eof,
        file.diagnostics().to_vec(),
    );
    let helper_emit = helpers::emit_helpers(&used_helpers, &options.helpers, Some(&rewritten));
    let prelude = join_preludes(&runtime_prelude, &helper_emit.prelude);
    let mut output = print_with_jsx_plan(
        &rewritten,
        model,
        PrintOptions {
            newline: options.newline,
            indent_width: options.indent_width,
            source_map: options.source_map,
            inline_source_map: options.inline_source_map,
        },
        names,
        Surface::JavaScript,
        Some(prelude),
        jsx_plan.as_ref(),
    );
    output.diagnostics.extend(plan.diagnostics);
    output.diagnostics.extend(jsx_diagnostics);
    output.diagnostics.extend(rewriter.diagnostics);
    output.diagnostics.extend(helper_emit.diagnostics);
    output.diagnostics.sort();
    output.diagnostics.dedup();
    output
}

fn executable_jsx_plan(
    file: &SourceFile,
    options: &TransformOptions,
    names: &EmitFileNames,
) -> Result<Option<JsxSourceDesugarPlan>, Box<Diagnostic>> {
    let Some(emit @ (JsxEmit::React | JsxEmit::ReactJsx | JsxEmit::ReactJsxDev)) = options.jsx
    else {
        return Ok(None);
    };
    let emit_options = JsxEmitOptions {
        emit,
        factory: options.jsx_factory.clone(),
        fragment_factory: options.jsx_fragment_factory.clone(),
        import_source: options.jsx_import_source.clone(),
        import_style: options.jsx_import_style,
        file_name: Some(Arc::clone(&names.source_name)),
    };
    let mut ids = NodeIdSource::after(file.id());
    desugar_source_jsx(file, file.source_text(), &emit_options, &mut ids)
        .map(Some)
        .map_err(|_| {
            Box::new(Diagnostic::error(
                super::codes::JSX_DESUGAR_FAILED,
                file.source_id(),
                file.range(),
                "JSX source desugaring failed",
            ))
        })
}

fn bind_and_render_jsx_runtime(file: &SourceFile, plan: &mut JsxSourceDesugarPlan) -> String {
    let Some(module) = plan.demand.module_specifier.clone() else {
        return String::new();
    };
    let demanded = plan
        .demand
        .bindings
        .values()
        .copied()
        .collect::<BTreeSet<_>>();
    if demanded.is_empty() {
        return String::new();
    }
    let mut occupied = file
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Identifier)
        .filter_map(|token| {
            let start = file
                .source_text()
                .utf16_to_byte(token.range().start())
                .ok()?;
            let end = file.source_text().utf16_to_byte(token.range().end()).ok()?;
            file.source_text()
                .as_str()
                .get(start..end)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let mut locals = BTreeMap::new();
    for binding in demanded {
        let preferred = match binding {
            JsxRuntimeBinding::Jsx => "_jsx",
            JsxRuntimeBinding::Jsxs => "_jsxs",
            JsxRuntimeBinding::JsxDev => "_jsxDEV",
            JsxRuntimeBinding::Fragment => "_Fragment",
        };
        let local = collision_free_name(preferred, &mut occupied);
        locals.insert(binding, Arc::<str>::from(local));
    }
    plan.rebind_runtime_names(&locals);

    let module = quote_module_specifier(&module);
    match plan.demand.import_style {
        JsxRuntimeImportStyle::EsModule => {
            let bindings = locals
                .iter()
                .map(|(binding, local)| format!("{} as {}", binding.export_name(), local))
                .collect::<Vec<_>>()
                .join(", ");
            format!("import {{ {bindings} }} from {module};\n")
        }
        JsxRuntimeImportStyle::CommonJs => {
            let namespace = collision_free_name("_jsxRuntime", &mut occupied);
            let mut prelude = format!("var {namespace} = require({module});\n");
            for (binding, local) in locals {
                prelude.push_str(&format!(
                    "var {local} = {namespace}.{};\n",
                    binding.export_name()
                ));
            }
            prelude
        }
    }
}

fn collision_free_name(preferred: &str, occupied: &mut BTreeSet<String>) -> String {
    if occupied.insert(preferred.to_owned()) {
        return preferred.to_owned();
    }
    let mut suffix = 1_u32;
    loop {
        let candidate = format!("{preferred}_{suffix}");
        if occupied.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn quote_module_specifier(module: &str) -> String {
    format!("\"{}\"", module.replace('\\', "\\\\").replace('"', "\\\""))
}

fn join_preludes(runtime: &str, helpers: &str) -> String {
    match (runtime.is_empty(), helpers.is_empty()) {
        (true, _) => helpers.to_owned(),
        (_, true) => runtime.to_owned(),
        (false, false) => format!("{runtime}{helpers}"),
    }
}

struct NameBank {
    text: String,
    utf16: usize,
    names: std::collections::BTreeMap<String, TextRange>,
}

impl NameBank {
    fn new(source: &SourceText) -> Self {
        Self {
            text: source.as_str().to_owned(),
            utf16: source.len_utf16().get(),
            names: std::collections::BTreeMap::new(),
        }
    }

    fn intern(&mut self, lexeme: &str) -> TextRange {
        if let Some(range) = self.names.get(lexeme) {
            return *range;
        }
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
            self.utf16 += 1;
        }
        let start = self.utf16;
        self.text.push_str(lexeme);
        self.utf16 += lexeme.encode_utf16().count();
        let range = TextRange::new(Utf16Pos::new(start), Utf16Pos::new(self.utf16))
            .expect("interned range is ordered");
        self.text.push('\n');
        self.utf16 += 1;
        self.names.insert(lexeme.to_owned(), range);
        range
    }
}

struct Rewriter<'a> {
    options: &'a TransformOptions,
    source_id: SourceId,
    bank: NameBank,
    next_id: u32,
    next_temp: u32,
    diagnostics: Vec<Diagnostic>,
    replace_await: bool,
    used_helpers: BTreeSet<HelperKind>,
}

impl<'a> Rewriter<'a> {
    fn new(file: &'a SourceFile, options: &'a TransformOptions) -> Self {
        Self {
            options,
            source_id: file.source_id(),
            bank: NameBank::new(file.source_text()),
            next_id: 1_000_000,
            next_temp: 0,
            diagnostics: Vec::new(),
            replace_await: false,
            used_helpers: BTreeSet::new(),
        }
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = NodeId::new(self.next_id);
        self.next_id += 1;
        id
    }

    fn ident(&mut self, name: &str) -> IdentifierNode {
        let range = self.bank.intern(name);
        let token = Token::new(TokenKind::Identifier, range);
        Node::new(self.alloc_id(), range, Identifier::new(token))
    }

    fn ident_expr(&mut self, name: &str) -> Expr {
        let ident = self.ident(name);
        let range = ident.range();
        Node::new(self.alloc_id(), range, Expression::Identifier(ident))
    }

    fn helper_ident(&mut self, kind: HelperKind) -> Expr {
        self.used_helpers.insert(kind);
        self.ident_expr(kind.ident())
    }

    fn number_expr(&mut self, lexeme: &str) -> Expr {
        let range = self.bank.intern(lexeme);
        let token = Token::new(TokenKind::NumericLiteral, range);
        let literal = Node::new(self.alloc_id(), range, NumericLiteral::new(token));
        Node::new(
            self.alloc_id(),
            range,
            Expression::Literal(Literal::Number(literal)),
        )
    }

    fn string_literal(&mut self, unquoted: &str) -> StringLiteralNode {
        let lexeme = format!("\"{unquoted}\"");
        let range = self.bank.intern(&lexeme);
        let token = Token::new(TokenKind::StringLiteral, range);
        Node::new(self.alloc_id(), range, StringLiteral::new(token))
    }

    fn temp_ident(&mut self) -> IdentifierNode {
        let name = format!("_t{}", self.next_temp);
        self.next_temp += 1;
        self.ident(&name)
    }

    fn node<T>(&mut self, range: TextRange, data: T) -> Node<T> {
        Node::new(self.alloc_id(), range, data)
    }

    fn diag(&mut self, code: DiagnosticCode, range: TextRange, message: &'static str) {
        self.diagnostics
            .push(Diagnostic::error(code, self.source_id, range, message));
    }

    fn finish_source(&self) -> Result<(Arc<SourceText>, Token, TextRange), SourcePositionError> {
        let source = Arc::new(SourceText::new(self.bank.text.clone())?);
        let len = source.len_utf16();
        let range =
            TextRange::new(Utf16Pos::ZERO, len).expect("source length is an ordered full range");
        let eof = Token::new(
            TokenKind::EndOfFile,
            TextRange::new(len, len).expect("equal source endpoints are ordered"),
        );
        Ok((source, eof, range))
    }

    fn needs(feature: LanguageFeature, options: &TransformOptions) -> bool {
        !options.target.supports(feature)
    }

    fn rewrite_statements(&mut self, statements: &[Stmt]) -> Vec<Stmt> {
        let mut out = Vec::new();
        for statement in statements {
            out.extend(self.rewrite_statement(statement));
        }
        out
    }

    fn rewrite_statement(&mut self, statement: &Stmt) -> Vec<Stmt> {
        match statement.data() {
            Statement::Class(class) => self.rewrite_class_statement(statement, class),
            Statement::Function(function) => {
                let function = self.rewrite_function_like(&function.function, statement.range());
                vec![self.node(
                    statement.range(),
                    Statement::Function(FunctionDeclaration { function }),
                )]
            }
            Statement::Variable(declaration) => {
                vec![self.rewrite_variable_statement(statement, declaration)]
            }
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                inner,
            ))) => {
                let rewritten = self.rewrite_statement(inner);
                rewritten
                    .into_iter()
                    .map(|inner| {
                        self.node(
                            statement.range(),
                            Statement::Export(ExportDeclaration::Named(
                                ExportNamedDeclaration::Declaration(Box::new(inner)),
                            )),
                        )
                    })
                    .collect()
            }
            Statement::Block(block) => {
                let statements = self.rewrite_statements(&block.data().statements);
                let block = self.node(block.range(), Block { statements });
                vec![self.node(statement.range(), Statement::Block(block))]
            }
            Statement::Return(ret) => {
                let argument = ret
                    .argument
                    .as_ref()
                    .map(|expression| Box::new(self.rewrite_expr(expression)));
                vec![self.node(
                    statement.range(),
                    Statement::Return(ReturnStatement { argument }),
                )]
            }
            Statement::Expression(expr) => {
                let expression = Box::new(self.rewrite_expr(&expr.expression));
                vec![self.node(
                    statement.range(),
                    Statement::Expression(ExpressionStatement { expression }),
                )]
            }
            Statement::ForOf(for_of) if for_of.mode == ForOfMode::Async => {
                if Self::needs(LanguageFeature::AsyncIteration, self.options) {
                    self.diag(
                        codes::ASYNC_ITERATION_REQUIRES_ES2018,
                        statement.range(),
                        "for-await-of requires ScriptTarget::Es2018 or later",
                    );
                }
                vec![statement.clone()]
            }
            _ => vec![statement.clone()],
        }
    }

    fn rewrite_variable_statement(
        &mut self,
        statement: &Stmt,
        declaration: &VariableDeclaration,
    ) -> Stmt {
        if matches!(
            declaration.kind,
            VariableKind::Using | VariableKind::AwaitUsing
        ) && Self::needs(LanguageFeature::Using, self.options)
        {
            self.diag(
                codes::USING_UNSUPPORTED,
                statement.range(),
                "using declarations are not downleveled",
            );
        }
        if !Self::needs(LanguageFeature::Destructuring, self.options) {
            return statement.clone();
        }
        let mut declarations = Vec::new();
        for declarator in &declaration.declarations {
            declarations.extend(self.lower_declarator(declaration.kind, declarator));
        }
        self.node(
            statement.range(),
            Statement::Variable(VariableDeclaration {
                range: declaration.range,
                kind: if self.options.target <= ScriptTarget::Es5 {
                    VariableKind::Var
                } else {
                    declaration.kind
                },
                declarations,
            }),
        )
    }

    fn lower_declarator(
        &mut self,
        kind: VariableKind,
        declarator: &VariableDeclaratorNode,
    ) -> Vec<VariableDeclaratorNode> {
        let _ = kind;
        let data = declarator.data();
        match data.binding.data() {
            BindingPattern::Identifier(_) => vec![declarator.clone()],
            BindingPattern::Object(object) => {
                self.lower_object_binding(declarator, object, data.initializer.as_deref())
            }
            BindingPattern::Array(array) => {
                self.lower_array_binding(declarator, array, data.initializer.as_deref())
            }
            _ => vec![declarator.clone()],
        }
    }

    fn rhs_ident(
        &mut self,
        initializer: Option<&Expr>,
        range: TextRange,
    ) -> (IdentifierNode, bool) {
        if let Some(expression) = initializer
            && let Expression::Identifier(ident) = expression.data()
        {
            return (ident.clone(), false);
        }
        let temp = self.temp_ident();
        let _ = range;
        (temp, true)
    }

    fn lower_object_binding(
        &mut self,
        declarator: &VariableDeclaratorNode,
        object: &ObjectBindingPattern,
        initializer: Option<&Expr>,
    ) -> Vec<VariableDeclaratorNode> {
        let range = declarator.range();
        let (rhs, needs_temp) = self.rhs_ident(initializer, range);
        let mut out = Vec::new();
        if needs_temp {
            let init = initializer
                .cloned()
                .unwrap_or_else(|| self.ident_expr("undefined"));
            out.push(self.make_declarator(rhs.clone(), Some(init), range));
        }
        let mut rest_names = Vec::new();
        for property in &object.properties {
            if let BindingPattern::Rest(rest) = property.binding.data() {
                if let BindingPattern::Identifier(ident) = rest.argument.data() {
                    let excluded = self.rest_exclude_literal(&rest_names, range);
                    let call = self.rest_call(&rhs, excluded, range);
                    out.push(self.make_declarator(ident.clone(), Some(call), range));
                }
                continue;
            }
            if let BindingPattern::Identifier(ident) = property.binding.data() {
                let key = property_key_text(property);
                rest_names.push(key.clone());
                let member = self.member_ident(&rhs, &key, range);
                out.push(self.make_declarator(ident.clone(), Some(member), range));
            }
        }
        out
    }

    fn lower_array_binding(
        &mut self,
        declarator: &VariableDeclaratorNode,
        array: &ArrayBindingPattern,
        initializer: Option<&Expr>,
    ) -> Vec<VariableDeclaratorNode> {
        let range = declarator.range();
        let (rhs, needs_temp) = self.rhs_ident(initializer, range);
        let mut out = Vec::new();
        if needs_temp {
            let init = initializer
                .cloned()
                .unwrap_or_else(|| self.ident_expr("undefined"));
            out.push(self.make_declarator(rhs.clone(), Some(init), range));
        }
        let mut index = 0usize;
        for element in &array.elements {
            match element {
                ArrayBindingElement::Elision => index += 1,
                ArrayBindingElement::Binding(binding) => match binding.data() {
                    BindingPattern::Identifier(ident) => {
                        let member = self.member_index(&rhs, index, range);
                        out.push(self.make_declarator(ident.clone(), Some(member), range));
                        index += 1;
                    }
                    BindingPattern::Rest(rest) => {
                        if let BindingPattern::Identifier(ident) = rest.argument.data() {
                            let slice = self.slice_call(&rhs, index, range);
                            out.push(self.make_declarator(ident.clone(), Some(slice), range));
                        }
                    }
                    _ => index += 1,
                },
                ArrayBindingElement::Missing(_) => index += 1,
            }
        }
        out
    }

    fn make_declarator(
        &mut self,
        ident: IdentifierNode,
        initializer: Option<Expr>,
        range: TextRange,
    ) -> VariableDeclaratorNode {
        let binding = self.node(ident.range(), BindingPattern::Identifier(ident));
        self.node(
            range,
            VariableDeclarator {
                binding,
                definite: false,
                type_annotation: None,
                initializer: initializer.map(Box::new),
            },
        )
    }

    fn member_ident(&mut self, object: &IdentifierNode, key: &str, range: TextRange) -> Expr {
        let object_expr = self.node(object.range(), Expression::Identifier(object.clone()));
        let property = self.ident(key);
        self.node(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object_expr),
                property: MemberProperty::Named(property),
                optional: false,
            }),
        )
    }

    fn member_index(&mut self, object: &IdentifierNode, index: usize, range: TextRange) -> Expr {
        let object_expr = self.node(object.range(), Expression::Identifier(object.clone()));
        let index_expr = self.number_expr(&index.to_string());
        self.node(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object_expr),
                property: MemberProperty::Computed(Box::new(index_expr)),
                optional: false,
            }),
        )
    }

    fn slice_call(&mut self, object: &IdentifierNode, start: usize, range: TextRange) -> Expr {
        let member = self.member_ident(object, "slice", range);
        let start_expr = self.number_expr(&start.to_string());
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(member),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Expression(Box::new(start_expr))],
            }),
        )
    }

    fn rest_exclude_literal(&mut self, names: &[String], range: TextRange) -> Expr {
        let mut elements = Vec::new();
        for name in names {
            let literal = self.string_literal(name);
            let expr = self.node(
                literal.range(),
                Expression::Literal(Literal::String(literal)),
            );
            elements.push(ArrayElement::Expression(Box::new(expr)));
        }
        let _ = range;
        self.node(range, Expression::Array(ArrayLiteral { elements }))
    }

    fn rest_call(&mut self, object: &IdentifierNode, excluded: Expr, range: TextRange) -> Expr {
        let callee = self.helper_ident(HelperKind::Rest);
        let object_expr = self.node(object.range(), Expression::Identifier(object.clone()));
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(object_expr)),
                    CallArgument::Expression(Box::new(excluded)),
                ],
            }),
        )
    }

    fn rewrite_class_statement(&mut self, statement: &Stmt, class: &ClassDeclaration) -> Vec<Stmt> {
        if Self::needs(LanguageFeature::Classes, self.options) {
            self.diag(
                codes::CLASS_REQUIRES_ES2015,
                statement.range(),
                "class syntax requires ScriptTarget::Es2015 or later",
            );
            return vec![statement.clone()];
        }
        let (class, statics) = self.rewrite_class(class, statement.range());
        let mut statements = vec![self.node(statement.range(), Statement::Class(class))];
        statements.extend(statics);
        statements
    }

    fn rewrite_class(
        &mut self,
        class: &ClassDeclaration,
        range: TextRange,
    ) -> (ClassDeclaration, Vec<Stmt>) {
        let lower_fields = Self::needs(LanguageFeature::ClassFields, self.options)
            && !self.options.use_define_for_class_fields;
        if !lower_fields {
            self.warn_private_fields(class);
            return (class.clone(), Vec::new());
        }
        let mut instance_inits = Vec::new();
        let mut static_inits = Vec::new();
        let mut kept_members = Vec::new();
        let mut constructor: Option<ClassMemberNode> = None;
        for member in &class.members {
            match member.data() {
                ClassMember::Property(property) => {
                    if matches!(property.name, PropertyName::Private(_)) {
                        self.diag(
                            codes::PRIVATE_FIELD_REQUIRES_ES2022,
                            member.range(),
                            "private class fields require ScriptTarget::Es2022 or later",
                        );
                        kept_members.push(member.clone());
                        continue;
                    }
                    if let Some(init) = &property.initializer {
                        if property.modifiers.is_static {
                            static_inits.push((property.name.clone(), self.rewrite_expr(init)));
                        } else {
                            instance_inits.push((property.name.clone(), self.rewrite_expr(init)));
                        }
                    } else {
                        kept_members.push(member.clone());
                    }
                }
                ClassMember::Constructor(_) => constructor = Some(member.clone()),
                _ => kept_members.push(member.clone()),
            }
        }
        let constructor = self.inject_instance_fields(
            constructor,
            &instance_inits,
            class.extends.is_some(),
            range,
        );
        let mut members = Vec::new();
        if let Some(constructor) = constructor {
            members.push(constructor);
        }
        members.extend(kept_members);
        let statics = self.static_assignments(class, &static_inits, range);
        (
            ClassDeclaration {
                members,
                ..class.clone()
            },
            statics,
        )
    }

    fn warn_private_fields(&mut self, class: &ClassDeclaration) {
        if self.options.target.supports(LanguageFeature::ClassFields) {
            return;
        }
        for member in &class.members {
            if let ClassMember::Property(property) = member.data()
                && matches!(property.name, PropertyName::Private(_))
            {
                self.diag(
                    codes::PRIVATE_FIELD_REQUIRES_ES2022,
                    member.range(),
                    "private class fields require ScriptTarget::Es2022 or later",
                );
            }
        }
    }

    fn inject_instance_fields(
        &mut self,
        constructor: Option<ClassMemberNode>,
        inits: &[(PropertyName, Expr)],
        derived: bool,
        range: TextRange,
    ) -> Option<ClassMemberNode> {
        if inits.is_empty() && constructor.is_none() {
            return None;
        }
        let assignments: Vec<Stmt> = inits
            .iter()
            .map(|(name, init)| self.this_assign(name, init, range))
            .collect();
        match constructor {
            Some(existing) => {
                let ClassMember::Constructor(ctor) = existing.data() else {
                    return Some(existing);
                };
                let mut statements = ctor.body.data().statements.clone();
                if derived {
                    let insertion = statements
                        .iter()
                        .position(is_super_call_statement)
                        .map_or(statements.len(), |index| index + 1);
                    statements.splice(insertion..insertion, assignments);
                } else {
                    statements.splice(0..0, assignments);
                }
                let body = self.node(ctor.body.range(), Block { statements });
                Some(self.node(
                    existing.range(),
                    ClassMember::Constructor(ConstructorDeclaration {
                        body,
                        ..ctor.clone()
                    }),
                ))
            }
            None => {
                let mut statements = Vec::new();
                if derived {
                    statements.push(self.super_forward_call(range));
                }
                statements.extend(assignments);
                let body = self.node(range, Block { statements });
                Some(self.node(
                    range,
                    ClassMember::Constructor(ConstructorDeclaration {
                        decorators: Vec::new(),
                        modifiers: DeclarationModifiers::default(),
                        parameters: Vec::new(),
                        body,
                    }),
                ))
            }
        }
    }

    fn this_assign(&mut self, name: &PropertyName, init: &Expr, range: TextRange) -> Stmt {
        let this_expr = self.node(range, Expression::This);
        let property = match name {
            PropertyName::Identifier(ident) => MemberProperty::Named(ident.clone()),
            PropertyName::Private(private) => MemberProperty::Private(private.clone()),
            PropertyName::Computed(expr) => MemberProperty::Computed(expr.clone()),
            PropertyName::String(literal) => MemberProperty::Computed(Box::new(self.node(
                literal.range(),
                Expression::Literal(Literal::String(literal.clone())),
            ))),
            PropertyName::Number(literal) => MemberProperty::Computed(Box::new(self.node(
                literal.range(),
                Expression::Literal(Literal::Number(literal.clone())),
            ))),
            PropertyName::Missing(_) => MemberProperty::Computed(Box::new(self.void_zero(range))),
        };
        let target = self.node(
            range,
            AssignmentTarget::Member(AssignmentMemberTarget {
                object: Box::new(this_expr),
                property,
            }),
        );
        let assignment = self.node(
            range,
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left: target,
                right: Box::new(init.clone()),
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(assignment),
            }),
        )
    }
    fn super_forward_call(&mut self, range: TextRange) -> Stmt {
        let arguments = self.ident_expr("arguments");
        let callee = self.node(range, Expression::Super);
        let call = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Spread(SpreadElement {
                    argument: Box::new(arguments),
                })],
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(call),
            }),
        )
    }

    fn static_assignments(
        &mut self,
        class: &ClassDeclaration,
        inits: &[(PropertyName, Expr)],
        range: TextRange,
    ) -> Vec<Stmt> {
        if inits.is_empty() {
            return Vec::new();
        }
        let Some(name) = &class.name else {
            self.diag(
                codes::STATIC_FIELD_REQUIRES_CLASS_NAME,
                range,
                "static fields on unnamed class expressions cannot be downleveled",
            );
            return Vec::new();
        };
        let mut statements = Vec::new();
        for (prop, init) in inits {
            let object = self.node(name.range(), Expression::Identifier(name.clone()));
            let property = match prop {
                PropertyName::Identifier(ident) => MemberProperty::Named(ident.clone()),
                other => {
                    let _ = other;
                    continue;
                }
            };
            let target = self.node(
                range,
                AssignmentTarget::Member(AssignmentMemberTarget {
                    object: Box::new(object),
                    property,
                }),
            );
            let assignment = self.node(
                range,
                Expression::Assignment(AssignmentExpression {
                    operator: AssignmentOperator::Assign,
                    left: target,
                    right: Box::new(init.clone()),
                }),
            );
            statements.push(self.node(
                range,
                Statement::Expression(ExpressionStatement {
                    expression: Box::new(assignment),
                }),
            ));
        }
        statements
    }

    fn rewrite_function_like(&mut self, function: &FunctionLike, range: TextRange) -> FunctionLike {
        if function.is_generator && Self::needs(LanguageFeature::Generators, self.options) {
            self.diag(
                codes::GENERATOR_REQUIRES_ES2015,
                range,
                "generators require ScriptTarget::Es2015 or later",
            );
        }
        if function.is_async && Self::needs(LanguageFeature::AsyncFunctions, self.options) {
            return self.lower_async_function(function, range);
        }
        let body = function.body.as_ref().map(|body| self.rewrite_body(body));
        FunctionLike {
            body,
            ..function.clone()
        }
    }

    fn rewrite_body(&mut self, body: &FunctionBody) -> FunctionBody {
        match body {
            FunctionBody::Block(block) => {
                let statements = self.rewrite_statements(&block.data().statements);
                FunctionBody::Block(self.node(block.range(), Block { statements }))
            }
            FunctionBody::Expression(expression) => {
                FunctionBody::Expression(Box::new(self.rewrite_expr(expression)))
            }
            FunctionBody::Missing(missing) => FunctionBody::Missing(missing.clone()),
        }
    }

    fn lower_async_function(&mut self, function: &FunctionLike, range: TextRange) -> FunctionLike {
        let previous = self.replace_await;
        self.replace_await = true;
        let inner_body = function
            .body
            .as_ref()
            .map(|body| self.rewrite_body(body))
            .unwrap_or_else(|| {
                FunctionBody::Block(self.node(
                    range,
                    Block {
                        statements: Vec::new(),
                    },
                ))
            });
        self.replace_await = previous;
        let inner = FunctionLike {
            decorators: Vec::new(),
            name: None,
            is_async: false,
            is_generator: true,
            type_parameters: None,
            parameters: Vec::new(),
            return_type: None,
            body: Some(inner_body),
        };
        let inner_expr = self.node(
            range,
            Expression::Function(FunctionExpression { function: inner }),
        );
        let awaiter = self.helper_ident(HelperKind::Awaiter);
        let this_arg = self.node(range, Expression::This);
        let void_zero = self.void_zero(range);
        let call = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(awaiter),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(this_arg)),
                    CallArgument::Expression(Box::new(void_zero.clone())),
                    CallArgument::Expression(Box::new(void_zero)),
                    CallArgument::Expression(Box::new(inner_expr)),
                ],
            }),
        );
        let return_stmt = self.node(
            range,
            Statement::Return(ReturnStatement {
                argument: Some(Box::new(call)),
            }),
        );
        FunctionLike {
            is_async: false,
            is_generator: false,
            body: Some(FunctionBody::Block(self.node(
                range,
                Block {
                    statements: vec![return_stmt],
                },
            ))),
            ..function.clone()
        }
    }

    fn void_zero(&mut self, range: TextRange) -> Expr {
        let zero = self.number_expr("0");
        self.node(
            range,
            Expression::Unary(UnaryExpression {
                operator: UnaryOperator::Void,
                argument: Box::new(zero),
            }),
        )
    }

    fn rewrite_expr(&mut self, expression: &Expr) -> Expr {
        if self.replace_await
            && let Expression::Await(awaited) = expression.data()
        {
            let argument = self.rewrite_expr(&awaited.argument);
            return self.node(
                expression.range(),
                Expression::Yield(YieldExpression {
                    delegate: false,
                    argument: Some(Box::new(argument)),
                }),
            );
        }
        match expression.data() {
            Expression::Function(function) => {
                let function = self.rewrite_function_like(&function.function, expression.range());
                self.node(
                    expression.range(),
                    Expression::Function(FunctionExpression { function }),
                )
            }
            Expression::Arrow(arrow) => self.rewrite_arrow(expression, arrow),
            Expression::Await(awaited) => {
                let argument = self.rewrite_expr(&awaited.argument);
                self.node(
                    expression.range(),
                    Expression::Await(AwaitExpression {
                        argument: Box::new(argument),
                    }),
                )
            }
            Expression::Call(call) => {
                let callee = self.rewrite_expr(&call.callee);
                self.node(
                    expression.range(),
                    Expression::Call(CallExpression {
                        callee: Box::new(callee),
                        optional: call.optional,
                        type_arguments: call.type_arguments.clone(),
                        arguments: call.arguments.clone(),
                    }),
                )
            }
            Expression::Assignment(assignment)
                if Self::needs(LanguageFeature::Destructuring, self.options)
                    && matches!(
                        assignment.left.data(),
                        AssignmentTarget::Object(_) | AssignmentTarget::Array(_)
                    ) =>
            {
                self.diag(
                    codes::DESTRUCTURING_ASSIGNMENT_REQUIRES_ES2015,
                    expression.range(),
                    "destructuring assignment requires ScriptTarget::Es2015 or later",
                );
                expression.clone()
            }
            _ => expression.clone(),
        }
    }

    fn rewrite_arrow(&mut self, expression: &Expr, arrow: &ArrowFunction) -> Expr {
        if arrow.is_async && Self::needs(LanguageFeature::AsyncFunctions, self.options) {
            let previous = self.replace_await;
            self.replace_await = true;
            let body = match &arrow.body {
                FunctionBody::Expression(expr) => {
                    let rewritten = self.rewrite_expr(expr);
                    let return_statement = self.node(
                        expression.range(),
                        Statement::Return(ReturnStatement {
                            argument: Some(Box::new(rewritten)),
                        }),
                    );
                    let block = self.node(
                        expression.range(),
                        Block {
                            statements: vec![return_statement],
                        },
                    );
                    FunctionBody::Block(block)
                }
                other => self.rewrite_body(other),
            };
            self.replace_await = previous;
            let inner = FunctionLike {
                decorators: Vec::new(),
                name: None,
                is_async: false,
                is_generator: true,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: None,
                body: Some(body),
            };
            let inner_expr = self.node(
                expression.range(),
                Expression::Function(FunctionExpression { function: inner }),
            );
            let awaiter = self.helper_ident(HelperKind::Awaiter);
            let this_arg = self.node(expression.range(), Expression::This);
            let void_zero = self.void_zero(expression.range());
            let call = self.node(
                expression.range(),
                Expression::Call(CallExpression {
                    callee: Box::new(awaiter),
                    optional: false,
                    type_arguments: None,
                    arguments: vec![
                        CallArgument::Expression(Box::new(this_arg)),
                        CallArgument::Expression(Box::new(void_zero.clone())),
                        CallArgument::Expression(Box::new(void_zero)),
                        CallArgument::Expression(Box::new(inner_expr)),
                    ],
                }),
            );
            return self.node(
                expression.range(),
                Expression::Arrow(ArrowFunction {
                    is_async: false,
                    type_parameters: None,
                    parameters: arrow.parameters.clone(),
                    return_type: None,
                    body: FunctionBody::Expression(Box::new(call)),
                }),
            );
        }
        expression.clone()
    }
}

fn is_super_call_statement(statement: &Stmt) -> bool {
    let Statement::Expression(expression) = statement.data() else {
        return false;
    };
    let Expression::Call(call) = expression.expression.data() else {
        return false;
    };
    matches!(call.callee.data(), Expression::Super)
}
fn property_key_text(property: &ObjectBindingProperty) -> String {
    match &property.name {
        PropertyName::Identifier(ident) => format!("id{}", ident.id().get()),
        _ => String::from("key"),
    }
}

fn scan_statements(
    file: &SourceFile,
    statements: &[Stmt],
    options: &TransformOptions,
    features: &mut BTreeSet<LanguageFeature>,
    diagnostics: &mut Vec<Diagnostic>,
    in_async: bool,
) {
    let _ = in_async;
    for statement in statements {
        match statement.data() {
            Statement::Class(class) => {
                features.insert(LanguageFeature::Classes);
                scan_class(file, class, options, features, diagnostics);
            }
            Statement::Function(function) => {
                scan_function(
                    file,
                    &function.function,
                    statement.range(),
                    options,
                    features,
                    diagnostics,
                );
            }
            Statement::Variable(declaration) => {
                if matches!(
                    declaration.kind,
                    VariableKind::Using | VariableKind::AwaitUsing
                ) {
                    features.insert(LanguageFeature::Using);
                }
                for declarator in &declaration.declarations {
                    scan_pattern(declarator.data().binding.data(), features);
                }
            }
            Statement::ForOf(for_of) => {
                if for_of.mode == ForOfMode::Async {
                    features.insert(LanguageFeature::AsyncIteration);
                }
            }
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                inner,
            ))) => scan_statements(
                file,
                std::slice::from_ref(inner),
                options,
                features,
                diagnostics,
                in_async,
            ),
            Statement::Block(block) => {
                scan_statements(
                    file,
                    &block.data().statements,
                    options,
                    features,
                    diagnostics,
                    in_async,
                );
            }
            _ => {}
        }
    }
}

fn scan_class(
    file: &SourceFile,
    class: &ClassDeclaration,
    options: &TransformOptions,
    features: &mut BTreeSet<LanguageFeature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for member in &class.members {
        match member.data() {
            ClassMember::Property(property) => {
                if property.initializer.is_some() {
                    features.insert(LanguageFeature::ClassFields);
                }
                if matches!(property.name, PropertyName::Private(_)) {
                    features.insert(LanguageFeature::ClassFields);
                    if !options.target.supports(LanguageFeature::ClassFields) {
                        diagnostics.push(Diagnostic::error(
                            codes::PRIVATE_FIELD_REQUIRES_ES2022,
                            file.source_id(),
                            member.range(),
                            "private class fields require ScriptTarget::Es2022 or later",
                        ));
                    }
                }
            }
            ClassMember::Method(method) => {
                scan_function(
                    file,
                    &method.function,
                    member.range(),
                    options,
                    features,
                    diagnostics,
                );
            }
            _ => {}
        }
    }
}

fn scan_function(
    file: &SourceFile,
    function: &FunctionLike,
    _range: TextRange,
    options: &TransformOptions,
    features: &mut BTreeSet<LanguageFeature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if function.is_async {
        features.insert(LanguageFeature::AsyncFunctions);
    }
    if function.is_generator {
        features.insert(LanguageFeature::Generators);
    }
    for parameter in &function.parameters {
        scan_pattern(parameter.data().binding.data(), features);
    }
    if let Some(FunctionBody::Block(body)) = &function.body {
        scan_statements(
            file,
            &body.data().statements,
            options,
            features,
            diagnostics,
            function.is_async,
        );
    }
}

fn scan_pattern(pattern: &BindingPattern, features: &mut BTreeSet<LanguageFeature>) {
    match pattern {
        BindingPattern::Object(_) | BindingPattern::Array(_) => {
            features.insert(LanguageFeature::Destructuring);
        }
        BindingPattern::Rest(_) => {
            features.insert(LanguageFeature::ObjectRestSpread);
            features.insert(LanguageFeature::Destructuring);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{LanguageFeature, ScriptTarget, analyze, codes, emit_transformed};
    use crate::checker;
    use crate::diagnostic::Recovered;
    use crate::emitter::{EmitFileNames, EmitOptions, EmitOutput};
    use crate::parser;
    use crate::scanner;
    use crate::source::{ScriptKind, SourceId, SourceText};
    use std::sync::Arc;

    fn parse(source: &str) -> crate::syntax::SourceFile {
        parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(SourceText::new(source).expect("test source fits the per-file source budget")),
        ))
        .into_product()
    }

    fn names() -> EmitFileNames {
        EmitFileNames {
            source_name: Arc::from("input.ts"),
            js_file_name: Some(Arc::from("output.js")),
            declaration_file_name: Some(Arc::from("output.d.ts")),
            source_root: None,
        }
    }

    fn options_for(target: ScriptTarget) -> EmitOptions {
        EmitOptions {
            target,
            ..EmitOptions::default()
        }
    }

    fn emit_at(source: &str, target: ScriptTarget) -> EmitOutput {
        let file = parse(source);
        let checked = checker::check(&Recovered::clean(parse(source)));
        let options = options_for(target);
        emit_transformed(
            &file,
            checked.product(),
            &options.transform_view(),
            &names(),
        )
    }

    fn javascript(output: &EmitOutput) -> &str {
        &output.javascript.as_ref().expect("JavaScript output").code
    }

    #[test]
    fn esnext_keeps_async_and_class_fields() {
        let source = "class C { x = 1; }\nasync function f() { return await 1; }\n";
        let output = emit_at(source, ScriptTarget::EsNext);
        let code = javascript(&output);
        assert!(code.contains("x = 1"), "{code}");
        assert!(code.contains("async function"), "{code}");
        assert!(!code.contains("__awaiter"), "{code}");
    }

    #[test]
    fn es2015_downlevels_class_fields_into_the_constructor() {
        let output = emit_at("class C { x = 1; }\n", ScriptTarget::Es2015);
        let code = javascript(&output);
        assert!(code.contains("constructor"), "{code}");
        assert!(code.contains("this.x = 1"), "{code}");
        assert!(
            !code.lines().any(|line| line.trim() == "x = 1;"),
            "field initializer should move: {code}"
        );
    }
    #[test]
    fn derived_class_fields_follow_super_and_literal_names_are_preserved() {
        let output = emit_at(
            r#"class C extends B { "x" = 1; 2 = 3; constructor() { super(); after(); } }
"#,
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let super_at = code
            .find("super()")
            .expect("derived constructor calls super");
        let string_at = code.find(r#"this["x"] = 1"#).expect("string field name");
        let number_at = code.find("this[2] = 3").expect("numeric field name");
        let after_at = code.find("after()").expect("original constructor body");
        assert!(
            super_at < string_at && string_at < number_at && number_at < after_at,
            "{code}"
        );
    }

    #[test]
    fn synthesized_derived_constructor_forwards_arguments_before_fields() {
        let output = emit_at("class C extends B { x = 1; }\n", ScriptTarget::Es2015);
        let code = javascript(&output);
        assert!(code.contains("super(...arguments)"), "{code}");
        assert!(
            code.find("super(...arguments)").unwrap() < code.find("this.x = 1").unwrap(),
            "{code}"
        );
    }

    #[test]
    fn es2015_downlevels_async_to_awaiter() {
        let output = emit_at(
            "async function f(x) { return await x; }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("__awaiter"), "{code}");
        assert!(code.contains("function*"), "{code}");
        assert!(code.contains("yield"), "{code}");
        assert!(!code.contains("async "), "{code}");
        assert!(code.contains("function __awaiter"), "{code}");
        assert!(code.contains("function __generator"), "{code}");
    }

    #[test]
    fn helper_emission_uses_rewriter_demand_not_analysis_prediction() {
        let source = "function outer() { return async () => await 1; }\n";
        let file = parse(source);
        let checked = checker::check(&Recovered::clean(parse(source)));
        let options = options_for(ScriptTarget::Es2015);
        let transform_options = options.transform_view();
        let plan = analyze(&file, &transform_options);
        assert!(
            plan.helpers.is_empty(),
            "fixture must remain outside the shallow analysis walk"
        );

        let output = emit_transformed(&file, checked.product(), &transform_options, &names());
        let code = javascript(&output);
        assert!(code.contains("function __awaiter"), "{code}");
        assert!(
            code.contains("function __generator"),
            "helper dependencies must close through the catalog: {code}"
        );
    }

    #[test]
    fn es5_downlevels_object_destructuring() {
        let output = emit_at("const { a, b } = o;\n", ScriptTarget::Es5);
        let code = javascript(&output);
        assert!(code.contains("a = "), "{code}");
        assert!(code.contains("b = "), "{code}");
        assert!(!code.contains("{ a, b }"), "{code}");
    }

    #[test]
    fn private_fields_below_es2022_are_errors() {
        let output = emit_at("class C { #x = 1; }\n", ScriptTarget::Es2015);
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::PRIVATE_FIELD_REQUIRES_ES2022),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn classes_below_es2015_are_errors() {
        let output = emit_at("class C {}\n", ScriptTarget::Es5);
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::CLASS_REQUIRES_ES2015),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn using_declarations_are_errors_before_esnext() {
        let output = emit_at("using x = y;\n", ScriptTarget::Es2022);
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::USING_UNSUPPORTED),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn analysis_is_deterministic_and_lists_required_features() {
        let file = parse("async function f() { const { a } = x; }\n");
        let options = options_for(ScriptTarget::Es5).transform_view();
        let left = analyze(&file, &options);
        let right = analyze(&file, &options);
        assert_eq!(left, right);
        assert!(left.required.contains(&LanguageFeature::AsyncFunctions));
        assert!(left.required.contains(&LanguageFeature::Destructuring));
    }

    #[test]
    fn transformed_output_is_byte_stable() {
        let source = "class C { x = 1; static y = 2; }\n";
        let file = parse(source);
        let checked = checker::check(&Recovered::clean(parse(source)));
        let options = options_for(ScriptTarget::Es2015).transform_view();
        let names = names();
        assert_eq!(
            emit_transformed(&file, checked.product(), &options, &names),
            emit_transformed(&file, checked.product(), &options, &names)
        );
    }
}
