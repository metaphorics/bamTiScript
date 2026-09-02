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
use super::{
    EmitFileNames, EmitOutput, ModuleKind, Newline, PrintOptions, PrintSource, Surface,
    print_with_jsx_plan,
};

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
    /// Auto-accessors below ES2022 require a dedicated transform.
    pub const AUTO_ACCESSOR_REQUIRES_ES2022: DiagnosticCode = DiagnosticCode::new("TS-EMIT-1109");
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
    Exponentiation,
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
            Self::Exponentiation => ScriptTarget::Es2016,
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
    /// Whether the emitted file begins with a `"use strict"` prologue.
    pub always_strict: bool,
    /// When true, class fields that survive downlevel use `define` semantics.
    /// Defaults to true for targets that natively include class fields.
    pub use_define_for_class_fields: bool,
    /// Helper import policy applied after rewrite.
    pub helpers: HelperOptions,
    /// Module kind used to decide strict prologue and JSX import style.
    pub module_kind: Option<ModuleKind>,
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
    /// Whether JavaScript source maps include the original source text.
    pub inline_sources: bool,
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

    let mut rewriter = Rewriter::new(file, options, Some(model));
    if jsx_plan
        .as_ref()
        .is_some_and(|plan| plan.demand.needs_assign)
    {
        rewriter.used_helpers.insert(HelperKind::Assign);
    }
    let statements = rewriter.rewrite_root_statements(file.statements(), file.range());
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
    // Under `moduleDetection: auto` a file whose JSX draws the automatic
    // runtime import is a module (commentsOnJSXExpressionsArePreserved),
    // so the synthesized import counts like a source-level one.
    let is_module = !runtime_prelude.is_empty() || crate::checker::source_is_module(file);
    let cjs_marker = rewriter.cjs_marker_prelude(!runtime_prelude.is_empty());
    let cjs_requires = rewriter.cjs_require_prelude();
    let prelude = join_preludes(
        &strict_prelude(file, options, is_module),
        &join_preludes(
            &helper_emit.prelude,
            &join_preludes(&cjs_marker, &join_preludes(&runtime_prelude, &cjs_requires)),
        ),
    );
    let mut output = print_with_jsx_plan(
        PrintSource {
            file: &rewritten,
            original_content: file.source_text().as_str(),
        },
        model,
        PrintOptions {
            newline: options.newline,
            indent_width: options.indent_width,
            source_map: options.source_map,
            inline_source_map: options.inline_source_map,
            inline_sources: options.inline_sources,
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

/// Builds the `"use strict";` prologue for one file.
///
/// Measured over 10781 single-file TypeScript 7.0.2 baselines carrying a real
/// `.js` section: a top-level external module takes the prologue exactly when
/// it is transpiled to CommonJS, and a script takes it exactly when
/// `alwaysStrict` is on. Agreement is 10694 of 10781.
///
/// Two edges that measurement settled against the obvious guess. An external
/// module emitted as ES modules takes no prologue even under `alwaysStrict`,
/// because the module format is already strict; emitting one there diverges on
/// 673 baselines. And AMD, UMD, and System take no head prologue either,
/// unlike CommonJS, which costs 238 baselines if they are lumped together.
/// No baseline sets `alwaysStrict` off on a CommonJS module, so that cell
/// follows the module rule rather than the option.
///
/// A source-level directive at the head suppresses the synthetic one.
#[must_use]
fn strict_prelude(file: &SourceFile, options: &TransformOptions, is_module: bool) -> String {
    // An unset `module` resolves the way upstream resolves it, from the target:
    // CommonJS below ES2015 and ES modules at or above it. That default decides
    // 673 baselines on its own, so it cannot be folded into either branch.
    let module_kind = options.module_kind.unwrap_or({
        if options.target < ScriptTarget::Es2015 {
            ModuleKind::CommonJs
        } else {
            ModuleKind::Es2015
        }
    });
    let strict = if is_module {
        module_kind == ModuleKind::CommonJs
    } else {
        options.always_strict
    };
    if !strict {
        return String::new();
    }
    let starts_with_strict = file.statements().first().is_some_and(|statement| {
        let Statement::Expression(statement) = statement.data() else {
            return false;
        };
        let Expression::Literal(Literal::String(literal)) = statement.expression.data() else {
            return false;
        };
        file.token_text(literal.data().token())
            .is_some_and(|text| text.trim_matches(['\'', '"']) == "use strict")
    });
    if starts_with_strict {
        String::new()
    } else {
        String::from("\"use strict\";\n")
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldMode {
    Native,
    Assign,
    Define,
}

impl FieldMode {
    const fn for_options(options: &TransformOptions) -> Self {
        if options.target.supports(LanguageFeature::ClassFields)
            && options.use_define_for_class_fields
        {
            Self::Native
        } else if options.use_define_for_class_fields {
            Self::Define
        } else {
            Self::Assign
        }
    }
}

#[derive(Clone)]
struct FieldInit {
    name: PropertyName,
    init: Expr,
    range: TextRange,
}

#[derive(Clone)]
enum StaticStep {
    Field(Box<FieldInit>),
    Block(BlockNode),
}

struct ClassLowering {
    class: ClassDeclaration,
    prelude: Vec<Stmt>,
    postlude: Vec<Stmt>,
}

#[derive(Clone, Copy)]
enum ExportWrapper {
    None,
    Named,
    Default,
}

struct Rewriter<'a> {
    options: &'a TransformOptions,
    model: Option<&'a SemanticModel>,
    source_id: SourceId,
    source_text: String,
    bank: NameBank,
    next_id: u32,
    next_temp: u32,
    diagnostics: Vec<Diagnostic>,
    replace_await: bool,
    used_helpers: BTreeSet<HelperKind>,
    key_prelude: Vec<Stmt>,
    expression_temp_scopes: Vec<Vec<IdentifierNode>>,
    parameter_initializer_depths: Vec<usize>,
    cjs: Option<CjsPlan>,
}

/// Per-file CommonJS lowering plan built before the rewrite walk.
#[derive(Default)]
struct CjsPlan {
    /// The file has module syntax, so it receives the `__esModule` marker.
    is_module: bool,
    /// Exported names needing `exports.<name> = void 0;` in the prelude.
    void_0: Vec<String>,
    /// `require` prelude lines in import-statement order.
    requires: Vec<String>,
    /// Import binding replacement by resolved module-scope symbol.
    imports: BTreeMap<crate::checker::SymbolId, CjsImportRef>,
    /// Require-local names whose member calls become `(0, m.p)()`.
    require_roots: BTreeSet<String>,
}

#[derive(Clone)]
struct CjsImportRef {
    /// The `require` binding local (`_10_lib_1`).
    root: String,
    /// Property read off the module object; `None` keeps the root bare.
    property: Option<String>,
}

/// Allocates `_module_N` require locals that avoid source identifiers.
struct RequireNames {
    occupied: BTreeSet<String>,
    counters: BTreeMap<String, u32>,
}

impl RequireNames {
    fn new(occupied: BTreeSet<String>) -> Self {
        Self {
            occupied,
            counters: BTreeMap::new(),
        }
    }

    fn allocate(&mut self, module: &str) -> String {
        let base = module.rsplit('/').next().unwrap_or(module);
        let counter = self.counters.entry(base.to_owned()).or_insert(0);
        loop {
            *counter += 1;
            let candidate = format!("_{base}_{}", *counter);
            if self.occupied.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

impl<'a> Rewriter<'a> {
    fn new(
        file: &'a SourceFile,
        options: &'a TransformOptions,
        model: Option<&'a SemanticModel>,
    ) -> Self {
        let cjs = if options.module_kind == Some(ModuleKind::CommonJs) {
            model.and_then(|model| Self::build_cjs_plan(file, options, model))
        } else {
            None
        };
        Self {
            options,
            model,
            source_id: file.source_id(),
            source_text: file.source_text().as_str().to_owned(),
            bank: NameBank::new(file.source_text()),
            next_id: 1_000_000,
            next_temp: 0,
            diagnostics: Vec::new(),
            replace_await: false,
            used_helpers: BTreeSet::new(),
            key_prelude: Vec::new(),
            expression_temp_scopes: Vec::new(),
            parameter_initializer_depths: Vec::new(),
            cjs,
        }
    }

    /// Scans top-level module syntax into a CommonJS lowering plan.
    fn build_cjs_plan(
        file: &SourceFile,
        options: &TransformOptions,
        model: &SemanticModel,
    ) -> Option<CjsPlan> {
        let occupied: BTreeSet<String> = file
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
            .collect();
        let mut names = RequireNames::new(occupied);
        let mut plan = CjsPlan::default();
        let module_scope = model.module_scope();
        let symbol_of = |name: &str| model.scope(module_scope).value(name);
        for statement in file.statements() {
            match statement.data() {
                Statement::Import(import) => {
                    if import.type_only {
                        continue;
                    }
                    plan.is_module = true;
                    let Some(clause) = &import.clause else {
                        continue;
                    };
                    let raw = raw_token_text(file, import.source.data().token());
                    let module = raw.trim_matches(['"', '\'']);
                    let local = names.allocate(module);
                    // Upstream re-quotes module specifiers with double
                    // quotes in every require form.
                    let quoted = quote_module_specifier(module);
                    let line = format!(
                        "{} {local} = require({quoted});",
                        if options.target <= ScriptTarget::Es5 {
                            "var"
                        } else {
                            "const"
                        }
                    );
                    plan.requires.push(line);
                    plan.require_roots.insert(local.clone());
                    if let Some(default) = &clause.default {
                        let text = identifier_text(file, default);
                        if let Some(symbol) = symbol_of(&text) {
                            plan.imports.insert(
                                symbol,
                                CjsImportRef {
                                    root: local.clone(),
                                    property: Some(String::from("default")),
                                },
                            );
                        }
                    }
                    match &clause.binding {
                        Some(ImportBinding::Namespace(name)) => {
                            let text = identifier_text(file, name);
                            if let Some(symbol) = symbol_of(&text) {
                                plan.imports.insert(
                                    symbol,
                                    CjsImportRef {
                                        root: local.clone(),
                                        property: None,
                                    },
                                );
                            }
                        }
                        Some(ImportBinding::Named(specifiers)) => {
                            for specifier in specifiers {
                                let specifier = specifier.data();
                                if !matches!(specifier.mode, ImportSpecifierMode::Value) {
                                    continue;
                                }
                                let imported = match &specifier.imported {
                                    ModuleExportName::Identifier(ident) => {
                                        Some(identifier_text(file, ident))
                                    }
                                    _ => None,
                                };
                                let Some(property) = imported else {
                                    continue;
                                };
                                let text = identifier_text(file, &specifier.local);
                                if let Some(symbol) = symbol_of(&text) {
                                    plan.imports.insert(
                                        symbol,
                                        CjsImportRef {
                                            root: local.clone(),
                                            property: Some(property),
                                        },
                                    );
                                }
                            }
                        }
                        None => {}
                    }
                }
                Statement::ImportEquals(import) => {
                    plan.is_module |= !import.is_type_only;
                }
                Statement::Export(export) => {
                    plan.is_module = true;
                    if let ExportDeclaration::Named(ExportNamedDeclaration::Declaration(inner)) =
                        export
                    {
                        match inner.data() {
                            Statement::Class(class) => {
                                if let Some(name) = &class.name {
                                    plan.void_0.push(identifier_text(file, name));
                                }
                            }
                            Statement::Function(_)
                            | Statement::Interface(_)
                            | Statement::TypeAlias(_) => {}
                            _ => {
                                for name in declared_binding_names(file, inner) {
                                    plan.void_0.push(name);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Script files keep the plan too: the marker stays off until a
        // require prelude (for example the JSX runtime) forces module form.
        plan.void_0.sort();
        plan.void_0.dedup();
        Some(plan)
    }

    /// `exports.<exported> = <local>;`
    fn cjs_export_assignment(&mut self, exported: &str, local: &str, range: TextRange) -> Stmt {
        let left = self.cjs_export_target(exported, range);
        let right = self.ident_expr(local);
        let assignment = self.node(
            range,
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left,
                right: Box::new(right),
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(assignment),
            }),
        )
    }

    /// `exports.default = <value>;`
    fn cjs_export_default_assignment(&mut self, range: TextRange, value: Expr) -> Stmt {
        let left = self.cjs_export_target("default", range);
        let assignment = self.node(
            range,
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left,
                right: Box::new(value),
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(assignment),
            }),
        )
    }

    fn cjs_export_target(&mut self, exported: &str, range: TextRange) -> AssignmentTargetNode {
        let exports_ident = self.ident("exports");
        let object = self.node(range, Expression::Identifier(exports_ident));
        let property = MemberProperty::Named(self.ident(exported));
        self.node(
            range,
            AssignmentTarget::Member(AssignmentMemberTarget {
                object: Box::new(object),
                property,
            }),
        )
    }

    /// `require("./m");` for a side-effect import, with the specifier
    /// re-quoted the way upstream prints module text.
    fn cjs_require_statement(&mut self, source: &StringLiteralNode) -> Stmt {
        let range = source.range();
        let raw = self.token_text(source.data().token()).to_owned();
        let module = raw.trim_matches(['"', '\'']);
        let require = self.ident("require");
        let callee = self.node(range, Expression::Identifier(require));
        let literal = self.string_literal(module);
        let module = self.node(range, Expression::Literal(Literal::String(literal)));
        let call = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Expression(Box::new(module))],
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(call),
            }),
        )
    }

    /// Replaces `export <declaration>` with assignments around the
    /// declaration. Functions hoist, so the export assignment precedes them;
    /// classes and variables receive the `void 0` preamble instead.
    fn cjs_export_declaration(&mut self, range: TextRange, inner: &Stmt) -> Vec<Stmt> {
        match inner.data() {
            Statement::Function(function) => {
                let Some(name) = function
                    .function
                    .name
                    .as_ref()
                    .map(|ident| self.token_text(ident.data().token()).to_owned())
                else {
                    return self.rewrite_statement(inner);
                };
                let mut out = vec![self.cjs_export_assignment(&name, &name, range)];
                out.extend(self.rewrite_statement(inner));
                out
            }
            Statement::Class(class) => {
                let Some(name) = class
                    .name
                    .as_ref()
                    .map(|ident| self.token_text(ident.data().token()).to_owned())
                else {
                    return self.rewrite_statement(inner);
                };
                let mut out = self.rewrite_statement(inner);
                out.push(self.cjs_export_assignment(&name, &name, range));
                out
            }
            _ => {
                let names = self.cjs_declared_names(inner);
                let mut out = self.rewrite_statement(inner);
                for name in &names {
                    out.push(self.cjs_export_assignment(name, name, range));
                }
                out
            }
        }
    }

    /// `export default class C {}` / anonymous `default_1` renaming.
    fn cjs_default_class(&mut self, range: TextRange, class: &ClassDeclaration) -> Vec<Stmt> {
        let (class, name) = match &class.name {
            Some(ident) => (
                class.clone(),
                self.token_text(ident.data().token()).to_owned(),
            ),
            None => {
                let ident = self.ident("default_1");
                let mut renamed = class.clone();
                renamed.name = Some(ident);
                (renamed, String::from("default_1"))
            }
        };
        let statement = self.node(range, Statement::Class(class));
        let mut out = self.rewrite_statement(&statement);
        let default_value = self.ident_expr(&name);
        out.push(self.cjs_export_default_assignment(range, default_value));
        out
    }

    /// `export { a, b as c };` becomes one assignment per specifier.
    fn cjs_export_specifiers(&mut self, specifiers: &[ExportSpecifierNode]) -> Vec<Stmt> {
        let mut out = Vec::new();
        for specifier in specifiers {
            let specifier = specifier.data();
            if matches!(specifier.mode, ExportSpecifierMode::TypeOnly) {
                continue;
            }
            let local_range = match &specifier.local {
                ModuleExportName::Identifier(local) => local.range(),
                _ => continue,
            };
            let (local, exported) = match (&specifier.local, &specifier.exported) {
                (ModuleExportName::Identifier(local), ModuleExportName::Identifier(exported)) => (
                    self.token_text(local.data().token()).to_owned(),
                    self.token_text(exported.data().token()).to_owned(),
                ),
                _ => continue,
            };
            out.push(self.cjs_export_assignment(&exported, &local, local_range));
        }
        out
    }

    /// Declared value names of a statement, via the rewrite text.
    fn cjs_declared_names(&self, statement: &Stmt) -> Vec<String> {
        match statement.data() {
            Statement::Variable(declaration) => declaration
                .declarations
                .iter()
                .filter_map(|declarator| match declarator.data().binding.data() {
                    BindingPattern::Identifier(ident) => {
                        Some(self.token_text(ident.data().token()).to_owned())
                    }
                    _ => None,
                })
                .collect(),
            Statement::Enum(declaration) => {
                vec![self.token_text(declaration.name.data().token()).to_owned()]
            }
            _ => Vec::new(),
        }
    }

    fn rewrite_export_named_declaration(&mut self, statement: &Stmt, inner: &Stmt) -> Vec<Stmt> {
        if self.cjs.is_some() {
            return self.cjs_export_declaration(statement.range(), inner);
        }
        match inner.data() {
            Statement::Class(class) => self.rewrite_named_export_class(statement, inner, class),
            _ => self
                .rewrite_statement(inner)
                .into_iter()
                .map(|inner| {
                    self.node(
                        statement.range(),
                        Statement::Export(ExportDeclaration::Named(
                            ExportNamedDeclaration::Declaration(Box::new(inner)),
                        )),
                    )
                })
                .collect(),
        }
    }
    /// `export default function f() {}` / anonymous `default_1` renaming.
    fn cjs_default_function(&mut self, range: TextRange, function: &FunctionLike) -> Vec<Stmt> {
        let (function, name) = match &function.name {
            Some(ident) => (
                function.clone(),
                self.token_text(ident.data().token()).to_owned(),
            ),
            None => {
                let ident = self.ident("default_1");
                let mut renamed = function.clone();
                renamed.name = Some(ident);
                (renamed, String::from("default_1"))
            }
        };
        let function = self.rewrite_function_like(&function, range);
        let statement = self.node(range, Statement::Function(FunctionDeclaration { function }));
        let mut out = vec![statement];
        let default_value = self.ident_expr(&name);
        out.push(self.cjs_export_default_assignment(range, default_value));
        out
    }

    /// Rewrites a resolved import reference to its CommonJS member form.
    fn cjs_identifier_replacement(&mut self, expression: &Expr) -> Option<Expr> {
        let (root, property) = {
            let plan = self.cjs.as_ref()?;
            let symbol = self.model?.reference(expression.id())?;
            let reference = plan.imports.get(&symbol)?;
            (reference.root.clone(), reference.property.clone())
        };
        let range = expression.range();
        let root_ident = self.ident(&root);
        let object = self.node(range, Expression::Identifier(root_ident));
        let Some(property) = property else {
            return Some(object);
        };
        let property = self.ident(&property);
        Some(self.node(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object),
                property: MemberProperty::Named(property),
                optional: false,
            }),
        ))
    }

    /// Whether a call's original callee identifier maps to an imported
    /// binding, so the rewritten member call becomes `(0, m.p)(...)`.
    fn cjs_call_needs_safe_wrap(&self, original_callee: &Expr) -> bool {
        let Some(plan) = &self.cjs else {
            return false;
        };
        let Expression::Identifier(ident) = original_callee.data() else {
            return false;
        };
        let Some(symbol) = self.model.and_then(|model| model.reference(ident.id())) else {
            return false;
        };
        plan.imports
            .get(&symbol)
            .is_some_and(|reference| reference.property.is_some())
    }

    fn cjs_wrap_callee(&mut self, callee: Expr) -> Expr {
        let range = callee.range();
        let zero = self.number_expr("0");
        self.node(
            range,
            Expression::Sequence(SequenceExpression {
                expressions: vec![zero, callee],
            }),
        )
    }

    /// The `__esModule` marker plus the `void 0` export preamble.
    fn cjs_marker_prelude(&self, force_module: bool) -> String {
        let Some(plan) = &self.cjs else {
            return String::new();
        };
        if !(plan.is_module || force_module) {
            return String::new();
        }
        let mut prelude =
            String::from("Object.defineProperty(exports, \"__esModule\", { value: true });\n");
        let mut names = plan.void_0.clone();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            prelude.push_str("exports.");
            for (index, name) in names.iter().enumerate() {
                if index > 0 {
                    prelude.push_str(" = exports.");
                }
                prelude.push_str(name);
            }
            prelude.push_str(" = void 0;\n");
        }
        prelude
    }

    fn cjs_require_prelude(&self) -> String {
        let Some(plan) = &self.cjs else {
            return String::new();
        };
        let mut prelude = String::new();
        for line in &plan.requires {
            prelude.push_str(line);
            prelude.push('\n');
        }
        prelude
    }

    /// Raw source text of a token, with escapes kept verbatim.
    fn token_text(&self, token: &Token) -> &str {
        let range = token.range();
        let start = range.start().get();
        let end = range.end().get();
        let start_byte = utf16_to_byte(&self.source_text, start);
        let end_byte = utf16_to_byte(&self.source_text, end);
        match (start_byte, end_byte) {
            (Some(start), Some(end)) => self.source_text.get(start..end).unwrap_or(""),
            _ => "",
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
    fn boolean_expr(&mut self, value: bool) -> Expr {
        let (lexeme, kind) = if value {
            ("true", TokenKind::KwTrue)
        } else {
            ("false", TokenKind::KwFalse)
        };
        let range = self.bank.intern(lexeme);
        let token = Token::new(kind, range);
        let literal = Node::new(self.alloc_id(), range, BooleanLiteral::new(token));
        Node::new(
            self.alloc_id(),
            range,
            Expression::Literal(Literal::Boolean(literal)),
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

    fn expression_temp(&mut self) -> IdentifierNode {
        let temp = self.temp_ident();
        if let Some(scope) = self.expression_temp_scopes.last_mut() {
            scope.push(temp.clone());
        }
        temp
    }

    fn temp_declaration(&mut self, temps: Vec<IdentifierNode>, range: TextRange) -> Option<Stmt> {
        if temps.is_empty() {
            return None;
        }
        let declarations = temps
            .into_iter()
            .map(|temp| self.make_declarator(temp, None, range))
            .collect();
        let declaration = VariableDeclaration {
            range,
            kind: VariableKind::Var,
            declarations,
        };
        Some(self.node(range, Statement::Variable(declaration)))
    }

    fn rewrite_root_statements(&mut self, statements: &[Stmt], range: TextRange) -> Vec<Stmt> {
        self.expression_temp_scopes.push(Vec::new());
        let mut rewritten = self.rewrite_statements(statements);
        let temps = self.expression_temp_scopes.pop().unwrap_or_default();
        if let Some(declaration) = self.temp_declaration(temps, range) {
            rewritten.insert(0, declaration);
        }
        rewritten
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
                self.rewrite_variable_statement(statement, declaration)
            }
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                inner,
            ))) => self.rewrite_export_named_declaration(statement, inner),
            Statement::Export(ExportDeclaration::Default(default)) => match &default.value {
                ExportDefaultValue::Class(class) => {
                    if self.cjs.is_some() {
                        return self.cjs_default_class(statement.range(), class);
                    }
                    self.rewrite_default_export_class(statement, class)
                }
                ExportDefaultValue::Function(function) => {
                    if self.cjs.is_some() {
                        return self.cjs_default_function(statement.range(), function);
                    }
                    vec![statement.clone()]
                }
                ExportDefaultValue::Expression(value) => {
                    if self.cjs.is_some() {
                        let value = self.rewrite_expr(value);
                        return vec![self.cjs_export_default_assignment(statement.range(), value)];
                    }
                    let value = self.rewrite_expr(value);
                    vec![self.node(
                        statement.range(),
                        Statement::Export(ExportDeclaration::Default(ExportDefaultDeclaration {
                            value: ExportDefaultValue::Expression(Box::new(value)),
                        })),
                    )]
                }
                ExportDefaultValue::Interface(_) | ExportDefaultValue::Missing(_) => {
                    vec![statement.clone()]
                }
            },
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                type_only,
                specifiers,
                source,
                ..
            })) => {
                if self.cjs.is_some() && source.is_none() && !*type_only {
                    return self.cjs_export_specifiers(specifiers);
                }
                vec![statement.clone()]
            }
            Statement::Import(import) => {
                if self.cjs.is_none() {
                    return vec![statement.clone()];
                }
                if import.type_only {
                    // Type-only imports carry no runtime form.
                    return Vec::new();
                }
                match &import.clause {
                    // A side-effect import stays at its position as a bare
                    // require; clause imports move into the prelude.
                    None => vec![self.cjs_require_statement(&import.source)],
                    Some(_) => Vec::new(),
                }
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
                let mut candidate = &*expr.expression;
                while let Expression::Parenthesized(value) = candidate.data() {
                    candidate = value;
                }
                if Self::needs(LanguageFeature::Destructuring, self.options)
                    && let Expression::Assignment(assignment) = candidate.data()
                    && matches!(
                        assignment.left.data(),
                        AssignmentTarget::Object(_) | AssignmentTarget::Array(_)
                    )
                {
                    return self.lower_assignment_destructuring(statement.range(), assignment);
                }
                let expression = Box::new(self.rewrite_expr(&expr.expression));
                vec![self.node(
                    statement.range(),
                    Statement::Expression(ExpressionStatement { expression }),
                )]
            }
            Statement::If(value) => {
                let test = self.rewrite_expr(&value.test);
                let consequent = self.rewrite_single_statement(&value.consequent);
                let alternate = value
                    .alternate
                    .as_deref()
                    .map(|statement| Box::new(self.rewrite_single_statement(statement)));
                vec![self.node(
                    statement.range(),
                    Statement::If(IfStatement {
                        test: Box::new(test),
                        consequent: Box::new(consequent),
                        alternate,
                    }),
                )]
            }
            Statement::While(value) => {
                let test = self.rewrite_expr(&value.test);
                let body = self.rewrite_single_statement(&value.body);
                vec![self.node(
                    statement.range(),
                    Statement::While(WhileStatement {
                        test: Box::new(test),
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::DoWhile(value) => {
                let body = self.rewrite_single_statement(&value.body);
                let test = self.rewrite_expr(&value.test);
                vec![self.node(
                    statement.range(),
                    Statement::DoWhile(DoWhileStatement {
                        body: Box::new(body),
                        test: Box::new(test),
                    }),
                )]
            }
            Statement::With(value) => {
                let object = self.rewrite_expr(&value.object);
                let body = self.rewrite_single_statement(&value.body);
                vec![self.node(
                    statement.range(),
                    Statement::With(WithStatement {
                        object: Box::new(object),
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::Labeled(value) => {
                let body = self.rewrite_single_statement(&value.body);
                vec![self.node(
                    statement.range(),
                    Statement::Labeled(LabeledStatement {
                        label: value.label.clone(),
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::Throw(value) => {
                let argument = self.rewrite_expr(&value.argument);
                vec![self.node(
                    statement.range(),
                    Statement::Throw(ThrowStatement {
                        argument: Box::new(argument),
                    }),
                )]
            }
            Statement::ForOf(for_of) => {
                if for_of.mode == ForOfMode::Async
                    && Self::needs(LanguageFeature::AsyncIteration, self.options)
                {
                    self.diag(
                        codes::ASYNC_ITERATION_REQUIRES_ES2018,
                        statement.range(),
                        "for-await-of requires ScriptTarget::Es2018 or later",
                    );
                }
                let iterable = self.rewrite_expr(&for_of.iterable);
                let body = self.rewrite_single_statement(&for_of.body);
                vec![self.node(
                    statement.range(),
                    Statement::ForOf(ForOfStatement {
                        mode: for_of.mode,
                        binding: for_of.binding.clone(),
                        iterable: Box::new(iterable),
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::ForIn(for_in) => {
                let object = self.rewrite_expr(&for_in.object);
                let body = self.rewrite_single_statement(&for_in.body);
                vec![self.node(
                    statement.range(),
                    Statement::ForIn(ForInStatement {
                        binding: for_in.binding.clone(),
                        object: Box::new(object),
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::For(value) => {
                let initializer = value
                    .initializer
                    .as_ref()
                    .map(|initializer| match initializer {
                        ForInitializer::Expression(expression) => {
                            ForInitializer::Expression(Box::new(self.rewrite_expr(expression)))
                        }
                        ForInitializer::Variable(declaration) => {
                            ForInitializer::Variable(declaration.clone())
                        }
                    });
                let test = value
                    .test
                    .as_deref()
                    .map(|expression| Box::new(self.rewrite_expr(expression)));
                let update = value
                    .update
                    .as_deref()
                    .map(|expression| Box::new(self.rewrite_expr(expression)));
                let body = self.rewrite_single_statement(&value.body);
                vec![self.node(
                    statement.range(),
                    Statement::For(ForStatement {
                        initializer,
                        test,
                        update,
                        body: Box::new(body),
                    }),
                )]
            }
            Statement::Try(value) => {
                let block = self.rewrite_block(&value.block);
                let handler = value.handler.as_ref().map(|handler| {
                    let body = self.rewrite_block(&handler.data().body);
                    self.node(
                        handler.range(),
                        CatchClause {
                            binding: handler.data().binding.clone(),
                            body,
                        },
                    )
                });
                let finalizer = value
                    .finalizer
                    .as_ref()
                    .map(|block| self.rewrite_block(block));
                vec![self.node(
                    statement.range(),
                    Statement::Try(TryStatement {
                        block,
                        handler,
                        finalizer,
                    }),
                )]
            }
            Statement::Switch(value) => {
                let discriminant = self.rewrite_expr(&value.discriminant);
                let cases = value
                    .cases
                    .iter()
                    .map(|case| {
                        let test = case
                            .data()
                            .test
                            .as_deref()
                            .map(|value| Box::new(self.rewrite_expr(value)));
                        let consequent = self.rewrite_statements(&case.data().consequent);
                        self.node(case.range(), SwitchCase { test, consequent })
                    })
                    .collect();
                vec![self.node(
                    statement.range(),
                    Statement::Switch(SwitchStatement {
                        discriminant: Box::new(discriminant),
                        cases,
                    }),
                )]
            }
            Statement::Declare(inner) => {
                let erased = match inner.data() {
                    Statement::Enum(_) | Statement::Namespace(_) => true,
                    Statement::Variable(declaration) => declaration.kind == VariableKind::Var,
                    _ => false,
                };
                if erased {
                    Vec::new()
                } else {
                    vec![statement.clone()]
                }
            }
            Statement::ImportEquals(_)
            | Statement::Export(_)
            | Statement::Interface(_)
            | Statement::TypeAlias(_)
            | Statement::Enum(_)
            | Statement::Namespace(_)
            | Statement::Empty
            | Statement::Break(_)
            | Statement::Continue(_)
            | Statement::Debugger
            | Statement::Missing(_) => vec![statement.clone()],
        }
    }

    fn rewrite_single_statement(&mut self, statement: &Stmt) -> Stmt {
        let statements = self.rewrite_statement(statement);
        if statements.len() == 1 {
            return statements
                .into_iter()
                .next()
                .expect("one rewritten statement");
        }
        let block = self.node(statement.range(), Block { statements });
        self.node(statement.range(), Statement::Block(block))
    }

    fn rewrite_block(&mut self, block: &BlockNode) -> BlockNode {
        let statements = self.rewrite_statements(&block.data().statements);
        self.node(block.range(), Block { statements })
    }

    fn rewrite_variable_statement(
        &mut self,
        statement: &Stmt,
        declaration: &VariableDeclaration,
    ) -> Vec<Stmt> {
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
            let declarations = declaration
                .declarations
                .iter()
                .map(|declarator| self.rewrite_declarator_initializer(declarator))
                .collect();
            return vec![self.node(
                statement.range(),
                Statement::Variable(VariableDeclaration {
                    declarations,
                    ..declaration.clone()
                }),
            )];
        }
        let mut declarations = Vec::new();
        self.key_prelude.clear();
        for declarator in &declaration.declarations {
            declarations.extend(self.lower_declarator(declaration.kind, declarator));
        }
        let key_prelude = std::mem::take(&mut self.key_prelude);
        let mut statements = key_prelude;
        statements.push(self.node(
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
        ));
        statements
    }

    fn lower_declarator(
        &mut self,
        kind: VariableKind,
        declarator: &VariableDeclaratorNode,
    ) -> Vec<VariableDeclaratorNode> {
        let _ = kind;
        let data = declarator.data();
        match data.binding.data() {
            BindingPattern::Identifier(_) => {
                vec![self.rewrite_declarator_initializer(declarator)]
            }
            BindingPattern::Object(object) => {
                self.lower_object_binding(declarator, object, data.initializer.as_deref())
            }
            BindingPattern::Array(array) => {
                self.lower_array_binding(declarator, array, data.initializer.as_deref())
            }
            BindingPattern::Assignment(_)
            | BindingPattern::Rest(_)
            | BindingPattern::Missing(_) => {
                vec![self.rewrite_declarator_initializer(declarator)]
            }
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

    fn rewrite_declarator_initializer(
        &mut self,
        declarator: &VariableDeclaratorNode,
    ) -> VariableDeclaratorNode {
        let initializer = declarator.data().initializer.as_deref().map(|value| {
            let rewritten = if let (BindingPattern::Identifier(binding), Expression::Class(class)) =
                (declarator.data().binding.data(), value.data())
            {
                let (source, _, _) = self.finish_source().expect("synthetic source");
                let range = binding.data().token().range();
                let start = source.utf16_to_byte(range.start()).expect("binding start");
                let end = source.utf16_to_byte(range.end()).expect("binding end");
                let name = source.as_str()[start..end].to_owned();
                self.lower_class_expression(value, class, Some(&name))
            } else {
                self.rewrite_expr(value)
            };
            Box::new(rewritten)
        });
        self.node(
            declarator.range(),
            VariableDeclarator {
                initializer,
                ..declarator.data().clone()
            },
        )
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
            let PropertyName::Computed(key) = &property.name else {
                continue;
            };
            let temp = self.temp_ident();
            let value = self.rewrite_expr(key);
            let declaration = self.make_temp_declaration(temp.clone(), value, range);
            self.key_prelude.push(declaration);
            if let BindingPattern::Identifier(ident) = property.binding.data() {
                let reference = self.node(temp.range(), Expression::Identifier(temp.clone()));
                let member = self.member_computed(&rhs, &reference, range);
                out.push(self.make_declarator(ident.clone(), Some(member), range));
            }
        }
        for property in &object.properties {
            if let PropertyName::Computed(_) = &property.name {
                continue;
            }
            if let BindingPattern::Rest(rest) = property.binding.data() {
                if let BindingPattern::Identifier(ident) = rest.argument.data() {
                    let excluded = self.rest_exclude_literal(&rest_names, range);
                    let call = self.rest_call(&rhs, excluded, range);
                    out.push(self.make_declarator(ident.clone(), Some(call), range));
                }
                continue;
            }
            if let BindingPattern::Identifier(ident) = property.binding.data() {
                let Some(key) = property_key_text(self, property) else {
                    continue;
                };
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

    /// `object[<computed expression>]`.
    fn member_computed(&mut self, object: &IdentifierNode, key: &Expr, range: TextRange) -> Expr {
        let object_expr = self.node(object.range(), Expression::Identifier(object.clone()));
        self.node(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object_expr),
                property: MemberProperty::Computed(Box::new(key.clone())),
                optional: false,
            }),
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

    /// `object.property` / `object[key]` over an arbitrary value expression.
    fn member_expr(&mut self, object: &Expr, property: MemberProperty, range: TextRange) -> Expr {
        self.node(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object.clone()),
                property,
                optional: false,
            }),
        )
    }

    /// Lowers one destructuring assignment at targets below ES2015 into a
    /// statement per assignment target. The right-hand side evaluates once,
    /// nested pattern values are captured in temps, and computed keys that
    /// are not bare identifiers evaluate once into a preceding temp.
    fn lower_assignment_destructuring(
        &mut self,
        range: TextRange,
        assignment: &AssignmentExpression,
    ) -> Vec<Stmt> {
        let mut statements = Vec::new();
        let value = if matches!(assignment.right.data(), Expression::Identifier(_)) {
            (*assignment.right).clone()
        } else {
            let temp = self.temp_ident();
            let right = self.rewrite_expr(&assignment.right);
            statements.push(self.make_temp_declaration(temp.clone(), right, range));
            self.node(temp.range(), Expression::Identifier(temp))
        };
        self.lower_assignment_target(&assignment.left, &value, &mut statements, range);
        statements
    }

    fn lower_assignment_target(
        &mut self,
        target: &AssignmentTargetNode,
        value: &Expr,
        statements: &mut Vec<Stmt>,
        range: TextRange,
    ) {
        match target.data() {
            AssignmentTarget::Identifier(_) | AssignmentTarget::Member(_) => {
                let assignment = Expression::Assignment(AssignmentExpression {
                    operator: AssignmentOperator::Assign,
                    left: target.clone(),
                    right: Box::new(value.clone()),
                });
                let expression = self.node(target.range(), assignment);
                statements.push(self.node(
                    target.range(),
                    Statement::Expression(ExpressionStatement {
                        expression: Box::new(expression),
                    }),
                ));
            }
            AssignmentTarget::Object(pattern) => {
                for property in &pattern.properties {
                    self.lower_assignment_property(property, value, statements, range);
                }
            }
            AssignmentTarget::Array(pattern) => {
                for (index, element) in pattern.elements.iter().enumerate() {
                    let AssignmentArrayElement::Target(inner) = element else {
                        continue;
                    };
                    let index_expr = self.number_expr(&index.to_string());
                    let member = self.member_expr(
                        value,
                        MemberProperty::Computed(Box::new(index_expr)),
                        range,
                    );
                    self.assign_or_recurse(inner, member, statements, range);
                }
            }
            AssignmentTarget::Missing(_) => {}
        }
    }

    /// Reads `member` once when `target` nests a pattern, otherwise assigns
    /// it directly.
    fn assign_or_recurse(
        &mut self,
        target: &AssignmentTargetNode,
        member: Expr,
        statements: &mut Vec<Stmt>,
        range: TextRange,
    ) {
        if matches!(
            target.data(),
            AssignmentTarget::Object(_) | AssignmentTarget::Array(_)
        ) {
            let temp = self.temp_ident();
            statements.push(self.make_temp_declaration(temp.clone(), member, range));
            let value = self.node(temp.range(), Expression::Identifier(temp));
            self.lower_assignment_target(target, &value, statements, range);
        } else {
            self.lower_assignment_target(target, &member, statements, range);
        }
    }

    fn lower_assignment_property(
        &mut self,
        property: &AssignmentObjectProperty,
        value: &Expr,
        statements: &mut Vec<Stmt>,
        range: TextRange,
    ) {
        let Some(member) = self.assignment_property_member(property, value, statements) else {
            // A folded spread element has no name or target; report it the
            // way the diagnostic for nested forms would.
            self.diag(
                codes::DESTRUCTURING_ASSIGNMENT_REQUIRES_ES2015,
                property_name_range(&property.name),
                "destructuring assignment requires ScriptTarget::Es2015 or later",
            );
            return;
        };
        if let Some(default) = &property.initializer {
            let read = self.temp_ident();
            statements.push(self.make_temp_declaration(read.clone(), member, range));
            let reference = self.node(read.range(), Expression::Identifier(read.clone()));
            let void_zero = self.void_zero(range);
            let test = self.node(
                range,
                Expression::Binary(BinaryExpression {
                    operator: BinaryOperator::StrictEqual,
                    left: Box::new(reference),
                    right: Box::new(void_zero),
                }),
            );
            let left = self.node(read.range(), AssignmentTarget::Identifier(read.clone()));
            let default_value = self.rewrite_expr(default);
            let assignment = Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left,
                right: Box::new(default_value),
            });
            let expression = self.node(range, assignment);
            let consequent = self.node(
                range,
                Statement::Expression(ExpressionStatement {
                    expression: Box::new(expression),
                }),
            );
            statements.push(self.node(
                range,
                Statement::If(IfStatement {
                    test: Box::new(test),
                    consequent: Box::new(consequent),
                    alternate: None,
                }),
            ));
            let read_value = self.node(read.range(), Expression::Identifier(read));
            self.assign_or_recurse(&property.target, read_value, statements, range);
        } else {
            self.assign_or_recurse(&property.target, member, statements, range);
        }
    }

    /// The `value[key]` read for one assignment-pattern property. String and
    /// number literal names keep their bracket form; a bare identifier
    /// computed key is pure and inlines, and any other computed key
    /// evaluates once into a preceding temp.
    fn assignment_property_member(
        &mut self,
        property: &AssignmentObjectProperty,
        value: &Expr,
        statements: &mut Vec<Stmt>,
    ) -> Option<Expr> {
        let range = property_name_range(&property.name);
        match &property.name {
            PropertyName::Identifier(ident) => {
                let key = self.token_text(ident.data().token()).to_owned();
                let property = self.ident(&key);
                Some(self.member_expr(value, MemberProperty::Named(property), range))
            }
            PropertyName::String(literal) => {
                let key = self.node(
                    literal.range(),
                    Expression::Literal(Literal::String(literal.clone())),
                );
                Some(self.member_expr(value, MemberProperty::Computed(Box::new(key)), range))
            }
            PropertyName::Number(literal) => {
                let key = self.node(
                    literal.range(),
                    Expression::Literal(Literal::Number(literal.clone())),
                );
                Some(self.member_expr(value, MemberProperty::Computed(Box::new(key)), range))
            }
            PropertyName::Computed(key) => {
                let key = self.rewrite_expr(key);
                let key = if matches!(key.data(), Expression::Identifier(_)) {
                    key
                } else {
                    let temp = self.temp_ident();
                    statements.push(self.make_temp_declaration(temp.clone(), key.clone(), range));
                    self.node(temp.range(), Expression::Identifier(temp))
                };
                Some(self.member_expr(value, MemberProperty::Computed(Box::new(key)), range))
            }
            PropertyName::Private(_) | PropertyName::Missing(_) => None,
        }
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
        self.rewrite_class_dispatch(statement, statement.range(), class, ExportWrapper::None)
    }

    fn rewrite_named_export_class(
        &mut self,
        statement: &Stmt,
        inner: &Stmt,
        class: &ClassDeclaration,
    ) -> Vec<Stmt> {
        self.rewrite_class_dispatch(statement, inner.range(), class, ExportWrapper::Named)
    }

    fn rewrite_default_export_class(
        &mut self,
        statement: &Stmt,
        class: &ClassDeclaration,
    ) -> Vec<Stmt> {
        self.rewrite_class_dispatch(statement, statement.range(), class, ExportWrapper::Default)
    }

    fn rewrite_class_dispatch(
        &mut self,
        statement: &Stmt,
        class_range: TextRange,
        class: &ClassDeclaration,
        wrapper: ExportWrapper,
    ) -> Vec<Stmt> {
        if Self::needs(LanguageFeature::Classes, self.options) {
            self.diag(
                codes::CLASS_REQUIRES_ES2015,
                statement.range(),
                "class syntax requires ScriptTarget::Es2015 or later",
            );
            return vec![statement.clone()];
        }
        let mut class = class.clone();
        let restored_name = if matches!(wrapper, ExportWrapper::Default) && class.name.is_none() {
            let name = self.temp_ident();
            class.name = Some(name.clone());
            Some(name)
        } else {
            None
        };
        let mut lowered = self.lower_class(&class, class_range, None);
        let class_statement = self.node(class_range, Statement::Class(lowered.class));
        let wrapped = match wrapper {
            ExportWrapper::None => class_statement,
            ExportWrapper::Named => self.node(
                statement.range(),
                Statement::Export(ExportDeclaration::Named(
                    ExportNamedDeclaration::Declaration(Box::new(class_statement)),
                )),
            ),
            ExportWrapper::Default => {
                let Statement::Class(class) = class_statement.data() else {
                    unreachable!("class dispatch must produce class");
                };
                self.node(
                    statement.range(),
                    Statement::Export(ExportDeclaration::Default(ExportDefaultDeclaration {
                        value: ExportDefaultValue::Class(class.clone()),
                    })),
                )
            }
        };
        lowered.prelude.push(wrapped);
        if let Some(name) = restored_name {
            lowered
                .prelude
                .push(self.restore_default_class_name(&name, statement.range()));
        }
        lowered.prelude.extend(lowered.postlude);
        lowered.prelude
    }

    fn lower_class(
        &mut self,
        class: &ClassDeclaration,
        range: TextRange,
        static_target: Option<&IdentifierNode>,
    ) -> ClassLowering {
        let mode = FieldMode::for_options(self.options);
        let mut class = class.clone();
        let mut prelude = Vec::new();
        let mut instance = Vec::new();
        let mut static_steps = Vec::new();
        let mut members = Vec::new();
        let mut constructor = None;
        let has_computed = class.members.iter().any(|member| {
            let name = match member.data() {
                ClassMember::Property(property) => Some(&property.name),
                ClassMember::Method(method) => Some(&method.name),
                ClassMember::AutoAccessor(accessor) => Some(&accessor.name),
                ClassMember::Constructor(_)
                | ClassMember::StaticBlock(_)
                | ClassMember::IndexSignature(_)
                | ClassMember::Missing(_) => None,
            };
            name.is_some_and(|name| matches!(name, PropertyName::Computed(_)))
        });
        if !matches!(mode, FieldMode::Native)
            && has_computed
            && let Some(heritage) = class.extends.as_mut()
        {
            let temp = self.temp_ident();
            let value = self.rewrite_expr(&heritage.expression);
            prelude.push(self.make_temp_declaration(
                temp.clone(),
                value,
                heritage.expression.range(),
            ));
            *heritage.expression = self.node(temp.range(), Expression::Identifier(temp));
        } else if let Some(heritage) = class.extends.as_mut() {
            *heritage.expression = self.rewrite_expr(&heritage.expression);
        }
        for member in &class.members {
            match member.data() {
                ClassMember::Constructor(ctor) => {
                    let mut ctor = ctor.clone();
                    let statements = self.rewrite_statements(&ctor.body.data().statements);
                    ctor.body = self.node(ctor.body.range(), Block { statements });
                    constructor = Some(self.node(member.range(), ClassMember::Constructor(ctor)));
                }
                ClassMember::Property(property)
                    if property.modifiers.is_declare || property.modifiers.is_abstract => {}
                ClassMember::Property(property) if matches!(mode, FieldMode::Native) => {
                    let mut property = property.clone();
                    property.name = self.rewrite_property_name(&property.name);
                    property.initializer = property
                        .initializer
                        .as_deref()
                        .map(|value| Box::new(self.rewrite_expr(value)));
                    members.push(self.node(member.range(), ClassMember::Property(property)));
                }
                ClassMember::Property(property) => {
                    if matches!(property.name, PropertyName::Private(_)) {
                        self.diag(
                            codes::PRIVATE_FIELD_REQUIRES_ES2022,
                            member.range(),
                            "private class fields require ScriptTarget::Es2022 or later",
                        );
                        members.push(member.clone());
                        continue;
                    }
                    let name =
                        self.lower_class_member_name(&property.name, &mut prelude, member.range());
                    let init = property
                        .initializer
                        .as_deref()
                        .map(|value| self.rewrite_expr(value))
                        .unwrap_or_else(|| self.void_zero(member.range()));
                    let field = FieldInit {
                        name,
                        init,
                        range: member.range(),
                    };
                    if property.modifiers.is_static {
                        static_steps.push(StaticStep::Field(Box::new(field)));
                    } else {
                        instance.push(field);
                    }
                }
                ClassMember::Method(method) => {
                    let mut method = method.clone();
                    method.name = if matches!(mode, FieldMode::Native) {
                        self.rewrite_property_name(&method.name)
                    } else {
                        self.lower_class_member_name(&method.name, &mut prelude, member.range())
                    };
                    method.function = self.rewrite_function_like(&method.function, member.range());
                    members.push(self.node(member.range(), ClassMember::Method(method)));
                }
                ClassMember::AutoAccessor(accessor) if matches!(mode, FieldMode::Native) => {
                    let mut accessor = accessor.clone();
                    accessor.name = self.rewrite_property_name(&accessor.name);
                    accessor.initializer = accessor
                        .initializer
                        .as_deref()
                        .map(|value| Box::new(self.rewrite_expr(value)));
                    members.push(self.node(member.range(), ClassMember::AutoAccessor(accessor)));
                }
                ClassMember::AutoAccessor(_) => {
                    self.diag(
                        codes::AUTO_ACCESSOR_REQUIRES_ES2022,
                        member.range(),
                        "auto-accessors require ScriptTarget::Es2022 or later",
                    );
                    members.push(member.clone());
                }
                ClassMember::StaticBlock(block) if matches!(mode, FieldMode::Native) => {
                    let statements = self.rewrite_statements(&block.data().statements);
                    let rewritten = self.node(block.range(), Block { statements });
                    members.push(self.node(member.range(), ClassMember::StaticBlock(rewritten)));
                }
                ClassMember::StaticBlock(block) => {
                    let statements = self.rewrite_statements(&block.data().statements);
                    static_steps.push(StaticStep::Block(
                        self.node(block.range(), Block { statements }),
                    ));
                }
                ClassMember::IndexSignature(_) | ClassMember::Missing(_) => {
                    members.push(member.clone());
                }
            }
        }
        debug_assert!(
            !has_computed || class.extends.is_none() || !prelude.is_empty(),
            "heritage must be captured before computed keys"
        );
        if matches!(mode, FieldMode::Native) {
            if let Some(ctor) = constructor {
                members.insert(0, ctor);
            }
        } else if let Some(ctor) =
            self.lower_constructor(constructor, &instance, class.extends.is_some(), range, mode)
        {
            members.insert(0, ctor);
        }
        class.members = members;
        let mut postlude = Vec::new();
        if !matches!(mode, FieldMode::Native)
            && let Some(name) = static_target.or(class.name.as_ref())
        {
            for step in static_steps {
                postlude.push(match step {
                    StaticStep::Field(mut field) => {
                        field.init = self.substitute_static_expr(&field.init, name);
                        self.field_statement(Some(name), &field, mode)
                    }
                    StaticStep::Block(block) => {
                        let block = self.substitute_static_block(&block, name);
                        self.static_block_statement(block, range)
                    }
                });
            }
        }
        ClassLowering {
            class,
            prelude,
            postlude,
        }
    }

    fn lower_class_member_name(
        &mut self,
        name: &PropertyName,
        prelude: &mut Vec<Stmt>,
        range: TextRange,
    ) -> PropertyName {
        let PropertyName::Computed(expression) = name else {
            return name.clone();
        };
        let temp = self.temp_ident();
        let value = self.rewrite_expr(expression);
        prelude.push(self.make_temp_declaration(temp.clone(), value, range));
        PropertyName::Computed(Box::new(
            self.node(temp.range(), Expression::Identifier(temp)),
        ))
    }

    fn rewrite_property_name(&mut self, name: &PropertyName) -> PropertyName {
        match name {
            PropertyName::Computed(value) => {
                PropertyName::Computed(Box::new(self.rewrite_expr(value)))
            }
            other => other.clone(),
        }
    }

    fn make_temp_declaration(
        &mut self,
        temp: IdentifierNode,
        value: Expr,
        range: TextRange,
    ) -> Stmt {
        let declaration = self.make_declarator(temp, Some(value), range);
        self.node(
            range,
            Statement::Variable(VariableDeclaration {
                range,
                kind: VariableKind::Let,
                declarations: vec![declaration],
            }),
        )
    }

    fn lower_constructor(
        &mut self,
        constructor: Option<ClassMemberNode>,
        fields: &[FieldInit],
        derived: bool,
        range: TextRange,
        mode: FieldMode,
    ) -> Option<ClassMemberNode> {
        if fields.is_empty() && constructor.is_none() {
            return None;
        }
        let inits: Vec<Stmt> = fields
            .iter()
            .map(|field| self.field_statement(None, field, mode))
            .collect();
        match constructor {
            Some(existing) => {
                let ClassMember::Constructor(ctor) = existing.data() else {
                    unreachable!("constructor slot must contain constructor");
                };
                let statements = if derived {
                    self.splice_after_super(&ctor.body.data().statements, &inits)
                } else {
                    let mut statements = inits;
                    statements.extend(ctor.body.data().statements.clone());
                    statements
                };
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
                statements.extend(inits);
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

    fn splice_after_super(&mut self, statements: &[Stmt], inits: &[Stmt]) -> Vec<Stmt> {
        let mut out = Vec::new();
        for statement in statements {
            match statement.data() {
                Statement::Expression(expr) if is_super_call_expression(&expr.expression) => {
                    out.push(statement.clone());
                    out.extend(inits.iter().cloned());
                }
                Statement::Block(block) => {
                    let statements = self.splice_after_super(&block.data().statements, inits);
                    let block = self.node(block.range(), Block { statements });
                    out.push(self.node(statement.range(), Statement::Block(block)));
                }
                Statement::If(if_stmt) => {
                    let consequent = self.splice_super_statement(&if_stmt.consequent, inits);
                    let alternate = if_stmt
                        .alternate
                        .as_deref()
                        .map(|value| Box::new(self.splice_super_statement(value, inits)));
                    out.push(self.node(
                        statement.range(),
                        Statement::If(IfStatement {
                            test: if_stmt.test.clone(),
                            consequent: Box::new(consequent),
                            alternate,
                        }),
                    ));
                }
                Statement::Try(try_stmt) => {
                    let statements =
                        self.splice_after_super(&try_stmt.block.data().statements, inits);
                    let block = self.node(try_stmt.block.range(), Block { statements });
                    let handler = try_stmt.handler.as_ref().map(|handler| {
                        let statements =
                            self.splice_after_super(&handler.data().body.data().statements, inits);
                        let body = self.node(handler.data().body.range(), Block { statements });
                        self.node(
                            handler.range(),
                            CatchClause {
                                binding: handler.data().binding.clone(),
                                body,
                            },
                        )
                    });
                    let finalizer = try_stmt.finalizer.as_ref().map(|block| {
                        let statements = self.splice_after_super(&block.data().statements, inits);
                        self.node(block.range(), Block { statements })
                    });
                    out.push(self.node(
                        statement.range(),
                        Statement::Try(TryStatement {
                            block,
                            handler,
                            finalizer,
                        }),
                    ));
                }
                Statement::For(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::For(ForStatement {
                            body: Box::new(body),
                            ..value.clone()
                        }),
                    ));
                }
                Statement::ForIn(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::ForIn(ForInStatement {
                            body: Box::new(body),
                            ..value.clone()
                        }),
                    ));
                }
                Statement::ForOf(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::ForOf(ForOfStatement {
                            body: Box::new(body),
                            ..value.clone()
                        }),
                    ));
                }
                Statement::While(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::While(WhileStatement {
                            body: Box::new(body),
                            ..value.clone()
                        }),
                    ));
                }
                Statement::DoWhile(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::DoWhile(DoWhileStatement {
                            body: Box::new(body),
                            ..value.clone()
                        }),
                    ));
                }
                Statement::Labeled(value) => {
                    let body = self.splice_super_statement(&value.body, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::Labeled(LabeledStatement {
                            label: value.label.clone(),
                            body: Box::new(body),
                        }),
                    ));
                }
                Statement::Switch(value) => {
                    let cases = value
                        .cases
                        .iter()
                        .map(|case| {
                            let consequent =
                                self.splice_after_super(&case.data().consequent, inits);
                            self.node(
                                case.range(),
                                SwitchCase {
                                    test: case.data().test.clone(),
                                    consequent,
                                },
                            )
                        })
                        .collect();
                    out.push(self.node(
                        statement.range(),
                        Statement::Switch(SwitchStatement {
                            discriminant: value.discriminant.clone(),
                            cases,
                        }),
                    ));
                }
                Statement::Expression(value) => {
                    let expression = self.wrap_super_calls(&value.expression, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::Expression(ExpressionStatement {
                            expression: Box::new(expression),
                        }),
                    ));
                }
                Statement::Return(value) => {
                    let argument = value
                        .argument
                        .as_deref()
                        .map(|value| Box::new(self.wrap_super_calls(value, inits)));
                    out.push(self.node(
                        statement.range(),
                        Statement::Return(ReturnStatement { argument }),
                    ));
                }
                Statement::Throw(value) => {
                    let argument = self.wrap_super_calls(&value.argument, inits);
                    out.push(self.node(
                        statement.range(),
                        Statement::Throw(ThrowStatement {
                            argument: Box::new(argument),
                        }),
                    ));
                }
                _ => out.push(statement.clone()),
            }
        }
        out
    }

    fn wrap_super_calls(&mut self, expression: &Expr, inits: &[Stmt]) -> Expr {
        if is_super_call_expression(expression) {
            let mut expressions = vec![expression.clone()];
            expressions.extend(inits.iter().filter_map(|statement| {
                let Statement::Expression(value) = statement.data() else {
                    return None;
                };
                Some(value.expression.as_ref().clone())
            }));
            expressions.push(self.node(expression.range(), Expression::This));
            return self.node(
                expression.range(),
                Expression::Sequence(SequenceExpression { expressions }),
            );
        }
        match expression.data() {
            Expression::Call(call) => {
                let callee = self.wrap_super_calls(&call.callee, inits);
                let arguments = call
                    .arguments
                    .iter()
                    .map(|argument| match argument {
                        CallArgument::Expression(value) => {
                            CallArgument::Expression(Box::new(self.wrap_super_calls(value, inits)))
                        }
                        CallArgument::Spread(spread) => CallArgument::Spread(SpreadElement {
                            argument: Box::new(self.wrap_super_calls(&spread.argument, inits)),
                        }),
                        CallArgument::Missing(missing) => CallArgument::Missing(missing.clone()),
                    })
                    .collect();
                self.node(
                    expression.range(),
                    Expression::Call(CallExpression {
                        callee: Box::new(callee),
                        optional: call.optional,
                        type_arguments: call.type_arguments.clone(),
                        arguments,
                    }),
                )
            }
            Expression::Conditional(value) => {
                let test = self.wrap_super_calls(&value.test, inits);
                let consequent = self.wrap_super_calls(&value.consequent, inits);
                let alternate = self.wrap_super_calls(&value.alternate, inits);
                self.node(
                    expression.range(),
                    Expression::Conditional(ConditionalExpression {
                        test: Box::new(test),
                        consequent: Box::new(consequent),
                        alternate: Box::new(alternate),
                    }),
                )
            }
            Expression::Binary(value) => {
                let left = self.wrap_super_calls(&value.left, inits);
                let right = self.wrap_super_calls(&value.right, inits);
                self.node(
                    expression.range(),
                    Expression::Binary(BinaryExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Logical(value) => {
                let left = self.wrap_super_calls(&value.left, inits);
                let right = self.wrap_super_calls(&value.right, inits);
                self.node(
                    expression.range(),
                    Expression::Logical(LogicalExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Sequence(value) => {
                let expressions = value
                    .expressions
                    .iter()
                    .map(|value| self.wrap_super_calls(value, inits))
                    .collect();
                self.node(
                    expression.range(),
                    Expression::Sequence(SequenceExpression { expressions }),
                )
            }
            Expression::Parenthesized(value) => {
                let value = self.wrap_super_calls(value, inits);
                self.node(
                    expression.range(),
                    Expression::Parenthesized(Box::new(value)),
                )
            }
            Expression::Assignment(value) => {
                let right = self.wrap_super_calls(&value.right, inits);
                self.node(
                    expression.range(),
                    Expression::Assignment(AssignmentExpression {
                        right: Box::new(right),
                        ..value.clone()
                    }),
                )
            }
            Expression::Arrow(arrow) => {
                let body = match &arrow.body {
                    FunctionBody::Expression(value) => {
                        FunctionBody::Expression(Box::new(self.wrap_super_calls(value, inits)))
                    }
                    FunctionBody::Block(block) => {
                        let statements = self.splice_after_super(&block.data().statements, inits);
                        FunctionBody::Block(self.node(block.range(), Block { statements }))
                    }
                    FunctionBody::Missing(missing) => FunctionBody::Missing(missing.clone()),
                };
                self.node(
                    expression.range(),
                    Expression::Arrow(ArrowFunction {
                        body,
                        ..arrow.clone()
                    }),
                )
            }
            Expression::Function(_) | Expression::Class(_) => expression.clone(),
            _ => expression.clone(),
        }
    }
    fn splice_super_statement(&mut self, statement: &Stmt, inits: &[Stmt]) -> Stmt {
        let statements = self.splice_after_super(std::slice::from_ref(statement), inits);
        if statements.len() == 1 {
            return statements.into_iter().next().expect("one statement");
        }
        let block = self.node(statement.range(), Block { statements });
        self.node(statement.range(), Statement::Block(block))
    }

    fn field_statement(
        &mut self,
        class_name: Option<&IdentifierNode>,
        field: &FieldInit,
        mode: FieldMode,
    ) -> Stmt {
        if matches!(mode, FieldMode::Define) {
            return self.define_field_statement(class_name, field);
        }
        let object = match class_name {
            Some(name) => self.node(name.range(), Expression::Identifier(name.clone())),
            None => self.node(field.range, Expression::This),
        };
        let property = self.member_property(&field.name, field.range);
        let target = self.node(
            field.range,
            AssignmentTarget::Member(AssignmentMemberTarget {
                object: Box::new(object),
                property,
            }),
        );
        let assignment = self.node(
            field.range,
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left: target,
                right: Box::new(field.init.clone()),
            }),
        );
        self.node(
            field.range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(assignment),
            }),
        )
    }

    fn define_field_statement(
        &mut self,
        class_name: Option<&IdentifierNode>,
        field: &FieldInit,
    ) -> Stmt {
        let object = self.ident("Object");
        let callee = self.member_ident(&object, "defineProperty", field.range);
        let receiver = match class_name {
            Some(name) => self.node(name.range(), Expression::Identifier(name.clone())),
            None => self.node(field.range, Expression::This),
        };
        let key = self.property_key_expression(&field.name, field.range);
        let enumerable = self.boolean_expr(true);
        let configurable = self.boolean_expr(true);
        let writable = self.boolean_expr(true);
        let descriptor = self.object_literal(
            vec![
                ("enumerable", enumerable),
                ("configurable", configurable),
                ("writable", writable),
                ("value", field.init.clone()),
            ],
            field.range,
        );
        let call = self.node(
            field.range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(receiver)),
                    CallArgument::Expression(Box::new(key)),
                    CallArgument::Expression(Box::new(descriptor)),
                ],
            }),
        );
        self.node(
            field.range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(call),
            }),
        )
    }

    fn member_property(&mut self, name: &PropertyName, range: TextRange) -> MemberProperty {
        match name {
            PropertyName::Identifier(name) => MemberProperty::Named(name.clone()),
            PropertyName::Private(name) => MemberProperty::Private(name.clone()),
            PropertyName::Computed(value) => MemberProperty::Computed(value.clone()),
            PropertyName::String(value) => MemberProperty::Computed(Box::new(self.node(
                value.range(),
                Expression::Literal(Literal::String(value.clone())),
            ))),
            PropertyName::Number(value) => MemberProperty::Computed(Box::new(self.node(
                value.range(),
                Expression::Literal(Literal::Number(value.clone())),
            ))),
            PropertyName::Missing(_) => MemberProperty::Computed(Box::new(self.void_zero(range))),
        }
    }

    fn property_key_expression(&mut self, name: &PropertyName, range: TextRange) -> Expr {
        match name {
            PropertyName::Identifier(name) => {
                let (source, _, _) = self.finish_source().expect("synthetic source");
                let range = name.data().token().range();
                let start = source
                    .utf16_to_byte(range.start())
                    .expect("identifier start is a source boundary");
                let end = source
                    .utf16_to_byte(range.end())
                    .expect("identifier end is a source boundary");
                let text = source.as_str()[start..end].to_owned();
                let literal = self.string_literal(&text);
                self.node(
                    literal.range(),
                    Expression::Literal(Literal::String(literal)),
                )
            }
            PropertyName::Private(_) => self.void_zero(range),
            PropertyName::Computed(value) => value.as_ref().clone(),
            PropertyName::String(value) => self.node(
                value.range(),
                Expression::Literal(Literal::String(value.clone())),
            ),
            PropertyName::Number(value) => self.node(
                value.range(),
                Expression::Literal(Literal::Number(value.clone())),
            ),
            PropertyName::Missing(_) => self.void_zero(range),
        }
    }

    fn object_literal(&mut self, entries: Vec<(&str, Expr)>, range: TextRange) -> Expr {
        let mut members = Vec::new();
        for (name, value) in entries {
            let property = ObjectProperty {
                name: PropertyName::Identifier(self.ident(name)),
                value: Box::new(value),
                modifier: PropertyModifier::None,
                shorthand: false,
            };
            members.push(self.node(range, ObjectMember::Property(property)));
        }
        self.node(range, Expression::Object(ObjectLiteral { members }))
    }

    fn substitute_static_block(
        &mut self,
        block: &BlockNode,
        class_name: &IdentifierNode,
    ) -> BlockNode {
        let statements = block
            .data()
            .statements
            .iter()
            .map(|statement| self.substitute_static_statement(statement, class_name))
            .collect();
        self.node(block.range(), Block { statements })
    }

    fn substitute_static_statement(
        &mut self,
        statement: &Stmt,
        class_name: &IdentifierNode,
    ) -> Stmt {
        match statement.data() {
            Statement::Expression(value) => {
                let expression = self.substitute_static_expr(&value.expression, class_name);
                self.node(
                    statement.range(),
                    Statement::Expression(ExpressionStatement {
                        expression: Box::new(expression),
                    }),
                )
            }
            Statement::Return(value) => {
                let argument = value.argument.as_deref().map(|expression| {
                    Box::new(self.substitute_static_expr(expression, class_name))
                });
                self.node(
                    statement.range(),
                    Statement::Return(ReturnStatement { argument }),
                )
            }
            Statement::Throw(value) => {
                let argument = self.substitute_static_expr(&value.argument, class_name);
                self.node(
                    statement.range(),
                    Statement::Throw(ThrowStatement {
                        argument: Box::new(argument),
                    }),
                )
            }
            Statement::Variable(value) => {
                let declarations = value
                    .declarations
                    .iter()
                    .map(|declaration| {
                        let initializer =
                            declaration.data().initializer.as_deref().map(|value| {
                                Box::new(self.substitute_static_expr(value, class_name))
                            });
                        self.node(
                            declaration.range(),
                            VariableDeclarator {
                                initializer,
                                ..declaration.data().clone()
                            },
                        )
                    })
                    .collect();
                self.node(
                    statement.range(),
                    Statement::Variable(VariableDeclaration {
                        declarations,
                        ..value.clone()
                    }),
                )
            }
            Statement::Block(block) => {
                let block = self.substitute_static_block(block, class_name);
                self.node(statement.range(), Statement::Block(block))
            }
            Statement::If(value) => {
                let test = self.substitute_static_expr(&value.test, class_name);
                let consequent = self.substitute_static_statement(&value.consequent, class_name);
                let alternate = value
                    .alternate
                    .as_deref()
                    .map(|value| Box::new(self.substitute_static_statement(value, class_name)));
                self.node(
                    statement.range(),
                    Statement::If(IfStatement {
                        test: Box::new(test),
                        consequent: Box::new(consequent),
                        alternate,
                    }),
                )
            }
            Statement::While(value) => {
                let test = self.substitute_static_expr(&value.test, class_name);
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::While(WhileStatement {
                        test: Box::new(test),
                        body: Box::new(body),
                    }),
                )
            }
            Statement::DoWhile(value) => {
                let body = self.substitute_static_statement(&value.body, class_name);
                let test = self.substitute_static_expr(&value.test, class_name);
                self.node(
                    statement.range(),
                    Statement::DoWhile(DoWhileStatement {
                        body: Box::new(body),
                        test: Box::new(test),
                    }),
                )
            }
            Statement::For(value) => {
                let initializer = value
                    .initializer
                    .as_ref()
                    .map(|initializer| match initializer {
                        ForInitializer::Expression(value) => ForInitializer::Expression(Box::new(
                            self.substitute_static_expr(value, class_name),
                        )),
                        ForInitializer::Variable(value) => ForInitializer::Variable(value.clone()),
                    });
                let test = value
                    .test
                    .as_deref()
                    .map(|value| Box::new(self.substitute_static_expr(value, class_name)));
                let update = value
                    .update
                    .as_deref()
                    .map(|value| Box::new(self.substitute_static_expr(value, class_name)));
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::For(ForStatement {
                        initializer,
                        test,
                        update,
                        body: Box::new(body),
                    }),
                )
            }
            Statement::ForIn(value) => {
                let object = self.substitute_static_expr(&value.object, class_name);
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::ForIn(ForInStatement {
                        object: Box::new(object),
                        body: Box::new(body),
                        ..value.clone()
                    }),
                )
            }
            Statement::ForOf(value) => {
                let iterable = self.substitute_static_expr(&value.iterable, class_name);
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::ForOf(ForOfStatement {
                        iterable: Box::new(iterable),
                        body: Box::new(body),
                        ..value.clone()
                    }),
                )
            }
            Statement::Labeled(value) => {
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::Labeled(LabeledStatement {
                        label: value.label.clone(),
                        body: Box::new(body),
                    }),
                )
            }
            Statement::Switch(value) => {
                let discriminant = self.substitute_static_expr(&value.discriminant, class_name);
                let cases = value
                    .cases
                    .iter()
                    .map(|case| {
                        let test =
                            case.data().test.as_deref().map(|value| {
                                Box::new(self.substitute_static_expr(value, class_name))
                            });
                        let consequent = case
                            .data()
                            .consequent
                            .iter()
                            .map(|value| self.substitute_static_statement(value, class_name))
                            .collect();
                        self.node(case.range(), SwitchCase { test, consequent })
                    })
                    .collect();
                self.node(
                    statement.range(),
                    Statement::Switch(SwitchStatement {
                        discriminant: Box::new(discriminant),
                        cases,
                    }),
                )
            }
            Statement::Try(value) => {
                let block = self.substitute_static_block(&value.block, class_name);
                let handler = value.handler.as_ref().map(|handler| {
                    let body = self.substitute_static_block(&handler.data().body, class_name);
                    self.node(
                        handler.range(),
                        CatchClause {
                            binding: handler.data().binding.clone(),
                            body,
                        },
                    )
                });
                let finalizer = value
                    .finalizer
                    .as_ref()
                    .map(|block| self.substitute_static_block(block, class_name));
                self.node(
                    statement.range(),
                    Statement::Try(TryStatement {
                        block,
                        handler,
                        finalizer,
                    }),
                )
            }
            Statement::With(value) => {
                let object = self.substitute_static_expr(&value.object, class_name);
                let body = self.substitute_static_statement(&value.body, class_name);
                self.node(
                    statement.range(),
                    Statement::With(WithStatement {
                        object: Box::new(object),
                        body: Box::new(body),
                    }),
                )
            }
            Statement::Function(_) | Statement::Class(_) => statement.clone(),
            Statement::Import(_)
            | Statement::ImportEquals(_)
            | Statement::Export(_)
            | Statement::Interface(_)
            | Statement::TypeAlias(_)
            | Statement::Enum(_)
            | Statement::Namespace(_)
            | Statement::Declare(_)
            | Statement::Empty
            | Statement::Break(_)
            | Statement::Continue(_)
            | Statement::Debugger
            | Statement::Missing(_) => statement.clone(),
        }
    }

    fn substitute_static_expr(&mut self, expression: &Expr, class_name: &IdentifierNode) -> Expr {
        match expression.data() {
            Expression::This => self.node(
                class_name.range(),
                Expression::Identifier(class_name.clone()),
            ),
            Expression::Member(member) if matches!(member.object.data(), Expression::Super) => {
                self.static_super_get(&member.property, class_name, expression.range())
            }
            Expression::Call(call)
                if matches!(
                    call.callee.data(),
                    Expression::Member(MemberExpression { object, .. })
                        if matches!(object.data(), Expression::Super)
                ) =>
            {
                let Expression::Member(member) = call.callee.data() else {
                    unreachable!("guarded super member");
                };
                let get = self.static_super_get(&member.property, class_name, expression.range());
                let call_name = self.ident("call");
                let callee = self.node(
                    expression.range(),
                    Expression::Member(MemberExpression {
                        object: Box::new(get),
                        property: MemberProperty::Named(call_name),
                        optional: false,
                    }),
                );
                let receiver = self.node(
                    class_name.range(),
                    Expression::Identifier(class_name.clone()),
                );
                let mut arguments = vec![CallArgument::Expression(Box::new(receiver))];
                arguments.extend(call.arguments.iter().map(|argument| match argument {
                    CallArgument::Expression(value) => CallArgument::Expression(Box::new(
                        self.substitute_static_expr(value, class_name),
                    )),
                    CallArgument::Spread(spread) => CallArgument::Spread(SpreadElement {
                        argument: Box::new(
                            self.substitute_static_expr(&spread.argument, class_name),
                        ),
                    }),
                    CallArgument::Missing(missing) => CallArgument::Missing(missing.clone()),
                }));
                self.node(
                    expression.range(),
                    Expression::Call(CallExpression {
                        callee: Box::new(callee),
                        optional: false,
                        type_arguments: None,
                        arguments,
                    }),
                )
            }
            Expression::Call(call) => {
                let callee = self.substitute_static_expr(&call.callee, class_name);
                let arguments = call
                    .arguments
                    .iter()
                    .map(|argument| match argument {
                        CallArgument::Expression(value) => CallArgument::Expression(Box::new(
                            self.substitute_static_expr(value, class_name),
                        )),
                        CallArgument::Spread(spread) => CallArgument::Spread(SpreadElement {
                            argument: Box::new(
                                self.substitute_static_expr(&spread.argument, class_name),
                            ),
                        }),
                        CallArgument::Missing(missing) => CallArgument::Missing(missing.clone()),
                    })
                    .collect();
                self.node(
                    expression.range(),
                    Expression::Call(CallExpression {
                        callee: Box::new(callee),
                        optional: call.optional,
                        type_arguments: call.type_arguments.clone(),
                        arguments,
                    }),
                )
            }
            Expression::Member(member) => {
                let object = self.substitute_static_expr(&member.object, class_name);
                let property = match &member.property {
                    MemberProperty::Computed(value) => MemberProperty::Computed(Box::new(
                        self.substitute_static_expr(value, class_name),
                    )),
                    other => other.clone(),
                };
                self.node(
                    expression.range(),
                    Expression::Member(MemberExpression {
                        object: Box::new(object),
                        property,
                        optional: member.optional,
                    }),
                )
            }
            Expression::Binary(value) => {
                let left = self.substitute_static_expr(&value.left, class_name);
                let right = self.substitute_static_expr(&value.right, class_name);
                self.node(
                    expression.range(),
                    Expression::Binary(BinaryExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Logical(value) => {
                let left = self.substitute_static_expr(&value.left, class_name);
                let right = self.substitute_static_expr(&value.right, class_name);
                self.node(
                    expression.range(),
                    Expression::Logical(LogicalExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Conditional(value) => {
                let test = self.substitute_static_expr(&value.test, class_name);
                let consequent = self.substitute_static_expr(&value.consequent, class_name);
                let alternate = self.substitute_static_expr(&value.alternate, class_name);
                self.node(
                    expression.range(),
                    Expression::Conditional(ConditionalExpression {
                        test: Box::new(test),
                        consequent: Box::new(consequent),
                        alternate: Box::new(alternate),
                    }),
                )
            }
            Expression::Assignment(value)
                if matches!(
                    value.left.data(),
                    AssignmentTarget::Member(AssignmentMemberTarget { object, .. })
                        if matches!(object.data(), Expression::Super)
                ) =>
            {
                let AssignmentTarget::Member(member) = value.left.data() else {
                    unreachable!("guarded static super assignment");
                };
                self.static_super_set(
                    &member.property,
                    value.operator,
                    &value.right,
                    class_name,
                    expression.range(),
                )
            }
            Expression::Assignment(value) => {
                let left = match value.left.data() {
                    AssignmentTarget::Member(member) => {
                        let object = self.substitute_static_expr(&member.object, class_name);
                        let property = match &member.property {
                            MemberProperty::Computed(value) => MemberProperty::Computed(Box::new(
                                self.substitute_static_expr(value, class_name),
                            )),
                            other => other.clone(),
                        };
                        self.node(
                            value.left.range(),
                            AssignmentTarget::Member(AssignmentMemberTarget {
                                object: Box::new(object),
                                property,
                            }),
                        )
                    }
                    _ => value.left.clone(),
                };
                let right = self.substitute_static_expr(&value.right, class_name);
                self.node(
                    expression.range(),
                    Expression::Assignment(AssignmentExpression {
                        operator: value.operator,
                        left,
                        right: Box::new(right),
                    }),
                )
            }
            _ => expression.clone(),
        }
    }

    fn static_super_set(
        &mut self,
        property: &MemberProperty,
        operator: AssignmentOperator,
        right: &Expr,
        class_name: &IdentifierNode,
        range: TextRange,
    ) -> Expr {
        let key_temp = self.temp_ident();
        let value_temp = self.temp_ident();
        let key = self.static_super_key(property, class_name, range);
        let key_statement = self.make_temp_declaration(key_temp.clone(), key, range);
        let key_reference = self.node(key_temp.range(), Expression::Identifier(key_temp.clone()));
        let temp_property = MemberProperty::Computed(Box::new(key_reference));
        let right = self.substitute_static_expr(right, class_name);
        let value = if operator == AssignmentOperator::Assign {
            right
        } else {
            let old = self.static_super_get(&temp_property, class_name, range);
            let binary_operator = match operator {
                AssignmentOperator::AddAssign => BinaryOperator::Add,
                AssignmentOperator::SubtractAssign => BinaryOperator::Subtract,
                AssignmentOperator::MultiplyAssign => BinaryOperator::Multiply,
                AssignmentOperator::DivideAssign => BinaryOperator::Divide,
                AssignmentOperator::RemainderAssign => BinaryOperator::Remainder,
                AssignmentOperator::ExponentiateAssign => BinaryOperator::Exponentiate,
                AssignmentOperator::LeftShiftAssign => BinaryOperator::LeftShift,
                AssignmentOperator::SignedRightShiftAssign => BinaryOperator::SignedRightShift,
                AssignmentOperator::UnsignedRightShiftAssign => BinaryOperator::UnsignedRightShift,
                AssignmentOperator::BitAndAssign => BinaryOperator::BitAnd,
                AssignmentOperator::BitXorAssign => BinaryOperator::BitXor,
                AssignmentOperator::BitOrAssign => BinaryOperator::BitOr,
                AssignmentOperator::LogicalAndAssign
                | AssignmentOperator::LogicalOrAssign
                | AssignmentOperator::NullishAssign
                | AssignmentOperator::Assign => {
                    return self.static_super_set(
                        property,
                        AssignmentOperator::Assign,
                        &right,
                        class_name,
                        range,
                    );
                }
            };
            self.node(
                range,
                Expression::Binary(BinaryExpression {
                    operator: binary_operator,
                    left: Box::new(old),
                    right: Box::new(right),
                }),
            )
        };
        let value_statement = self.make_temp_declaration(value_temp.clone(), value, range);
        let value_reference = self.node(
            value_temp.range(),
            Expression::Identifier(value_temp.clone()),
        );
        let set = self.static_super_reflect_set(
            temp_property,
            value_reference.clone(),
            class_name,
            range,
        );
        let set_statement = self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(set),
            }),
        );
        let return_statement = self.node(
            range,
            Statement::Return(ReturnStatement {
                argument: Some(Box::new(value_reference)),
            }),
        );
        let body = self.node(
            range,
            Block {
                statements: vec![
                    key_statement,
                    value_statement,
                    set_statement,
                    return_statement,
                ],
            },
        );
        let arrow = self.node(
            range,
            Expression::Arrow(ArrowFunction {
                is_async: false,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: None,
                body: FunctionBody::Block(body),
            }),
        );
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(arrow),
                optional: false,
                type_arguments: None,
                arguments: Vec::new(),
            }),
        )
    }

    fn static_super_key(
        &mut self,
        property: &MemberProperty,
        class_name: &IdentifierNode,
        range: TextRange,
    ) -> Expr {
        match property {
            MemberProperty::Named(name) => {
                let (source, _, _) = self.finish_source().expect("synthetic source");
                let token_range = name.data().token().range();
                let start = source
                    .utf16_to_byte(token_range.start())
                    .expect("name start");
                let end = source.utf16_to_byte(token_range.end()).expect("name end");
                let text = source.as_str()[start..end].to_owned();
                let literal = self.string_literal(&text);
                self.node(
                    literal.range(),
                    Expression::Literal(Literal::String(literal)),
                )
            }
            MemberProperty::Private(_) => self.void_zero(range),
            MemberProperty::Computed(value) => self.substitute_static_expr(value, class_name),
        }
    }

    fn static_super_reflect_set(
        &mut self,
        property: MemberProperty,
        value: Expr,
        class_name: &IdentifierNode,
        range: TextRange,
    ) -> Expr {
        let object = self.ident("Object");
        let get_proto = self.member_ident(&object, "getPrototypeOf", range);
        let class = self.node(
            class_name.range(),
            Expression::Identifier(class_name.clone()),
        );
        let proto = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(get_proto),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Expression(Box::new(class.clone()))],
            }),
        );
        let key = match property {
            MemberProperty::Named(_name) => {
                let literal = self.string_literal("name");
                self.node(
                    literal.range(),
                    Expression::Literal(Literal::String(literal)),
                )
            }
            MemberProperty::Private(_) => self.void_zero(range),
            MemberProperty::Computed(value) => value.as_ref().clone(),
        };
        let reflect = self.ident("Reflect");
        let set = self.member_ident(&reflect, "set", range);
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(set),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(proto)),
                    CallArgument::Expression(Box::new(key)),
                    CallArgument::Expression(Box::new(value)),
                    CallArgument::Expression(Box::new(class)),
                ],
            }),
        )
    }
    fn static_super_get(
        &mut self,
        property: &MemberProperty,
        class_name: &IdentifierNode,
        range: TextRange,
    ) -> Expr {
        let object = self.ident("Object");
        let get_proto = self.member_ident(&object, "getPrototypeOf", range);
        let class = self.node(
            class_name.range(),
            Expression::Identifier(class_name.clone()),
        );
        let proto = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(get_proto),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Expression(Box::new(class.clone()))],
            }),
        );
        let key = match property {
            MemberProperty::Named(name) => {
                let (source, _, _) = self.finish_source().expect("synthetic source");
                let token_range = name.data().token().range();
                let start = source
                    .utf16_to_byte(token_range.start())
                    .expect("name start");
                let end = source.utf16_to_byte(token_range.end()).expect("name end");
                let text = source.as_str()[start..end].to_owned();
                let literal = self.string_literal(&text);
                self.node(
                    literal.range(),
                    Expression::Literal(Literal::String(literal)),
                )
            }
            MemberProperty::Private(_) => self.void_zero(range),
            MemberProperty::Computed(value) => self.substitute_static_expr(value, class_name),
        };
        let reflect = self.ident("Reflect");
        let get = self.member_ident(&reflect, "get", range);
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(get),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(proto)),
                    CallArgument::Expression(Box::new(key)),
                    CallArgument::Expression(Box::new(class)),
                ],
            }),
        )
    }
    fn static_block_statement(&mut self, block: BlockNode, range: TextRange) -> Stmt {
        let arrow = self.node(
            range,
            Expression::Arrow(ArrowFunction {
                is_async: false,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: None,
                body: FunctionBody::Block(block),
            }),
        );
        let call = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(arrow),
                optional: false,
                type_arguments: None,
                arguments: Vec::new(),
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(call),
            }),
        )
    }

    fn restore_default_class_name(
        &mut self,
        class_name: &IdentifierNode,
        range: TextRange,
    ) -> Stmt {
        self.restore_class_name(class_name, "default", range)
    }

    fn restore_class_name(
        &mut self,
        class_name: &IdentifierNode,
        value: &str,
        range: TextRange,
    ) -> Stmt {
        let object = self.ident("Object");
        let callee = self.member_ident(&object, "defineProperty", range);
        let receiver = self.node(
            class_name.range(),
            Expression::Identifier(class_name.clone()),
        );
        let key_literal = self.string_literal("name");
        let key = self.node(
            key_literal.range(),
            Expression::Literal(Literal::String(key_literal)),
        );
        let value_literal = self.string_literal(value);
        let value = self.node(
            value_literal.range(),
            Expression::Literal(Literal::String(value_literal)),
        );
        let configurable = self.boolean_expr(true);
        let descriptor = self.object_literal(
            vec![("value", value), ("configurable", configurable)],
            range,
        );
        let call = self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(receiver)),
                    CallArgument::Expression(Box::new(key)),
                    CallArgument::Expression(Box::new(descriptor)),
                ],
            }),
        );
        self.node(
            range,
            Statement::Expression(ExpressionStatement {
                expression: Box::new(call),
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

    fn rewrite_function_like(&mut self, function: &FunctionLike, range: TextRange) -> FunctionLike {
        if function.is_generator && Self::needs(LanguageFeature::Generators, self.options) {
            self.diag(
                codes::GENERATOR_REQUIRES_ES2015,
                range,
                "generators require ScriptTarget::Es2015 or later",
            );
        }
        let parameters = self.rewrite_parameters(&function.parameters);
        if function.is_async && Self::needs(LanguageFeature::AsyncFunctions, self.options) {
            let mut rewritten = function.clone();
            rewritten.parameters = parameters;
            return self.lower_async_function(&rewritten, range);
        }
        let body = function.body.as_ref().map(|body| self.rewrite_body(body));
        FunctionLike {
            parameters,
            body,
            ..function.clone()
        }
    }

    fn rewrite_parameters(&mut self, parameters: &[ParameterNode]) -> Vec<ParameterNode> {
        parameters
            .iter()
            .map(|parameter| {
                let initializer = parameter.data().initializer.as_deref().map(|initializer| {
                    let depth = self.expression_temp_scopes.len();
                    self.parameter_initializer_depths.push(depth);
                    let rewritten = self.rewrite_expr(initializer);
                    let popped = self.parameter_initializer_depths.pop();
                    debug_assert_eq!(popped, Some(depth));
                    Box::new(rewritten)
                });
                self.node(
                    parameter.range(),
                    Parameter {
                        initializer,
                        ..parameter.data().clone()
                    },
                )
            })
            .collect()
    }

    fn rewrite_body(&mut self, body: &FunctionBody) -> FunctionBody {
        match body {
            FunctionBody::Block(block) => {
                self.expression_temp_scopes.push(Vec::new());
                let mut statements = self.rewrite_statements(&block.data().statements);
                let temps = self.expression_temp_scopes.pop().unwrap_or_default();
                if let Some(declaration) = self.temp_declaration(temps, block.range()) {
                    statements.insert(0, declaration);
                }
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

    fn lower_class_expression(
        &mut self,
        expression: &Expr,
        class_expression: &ClassExpression,
        inferred_name: Option<&str>,
    ) -> Expr {
        if matches!(FieldMode::for_options(self.options), FieldMode::Native) {
            let lowered = self.lower_class(&class_expression.class, expression.range(), None);
            return self.node(
                expression.range(),
                Expression::Class(ClassExpression {
                    class: lowered.class,
                }),
            );
        }
        let temp = self.temp_ident();
        let mut lowered =
            self.lower_class(&class_expression.class, expression.range(), Some(&temp));
        let class = self.node(
            expression.range(),
            Expression::Class(ClassExpression {
                class: lowered.class,
            }),
        );
        let declaration = self.make_declarator(temp.clone(), Some(class), expression.range());
        lowered.prelude.push(self.node(
            expression.range(),
            Statement::Variable(VariableDeclaration {
                range: expression.range(),
                kind: VariableKind::Let,
                declarations: vec![declaration],
            }),
        ));
        if class_expression.class.name.is_none() {
            lowered.prelude.push(self.restore_class_name(
                &temp,
                inferred_name.unwrap_or(""),
                expression.range(),
            ));
        }
        lowered.prelude.extend(lowered.postlude);
        let result = self.node(temp.range(), Expression::Identifier(temp));
        lowered.prelude.push(self.node(
            expression.range(),
            Statement::Return(ReturnStatement {
                argument: Some(Box::new(result)),
            }),
        ));
        let body = self.node(
            expression.range(),
            Block {
                statements: lowered.prelude,
            },
        );
        let arrow = self.node(
            expression.range(),
            Expression::Arrow(ArrowFunction {
                is_async: false,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: None,
                body: FunctionBody::Block(body),
            }),
        );
        self.node(
            expression.range(),
            Expression::Call(CallExpression {
                callee: Box::new(arrow),
                optional: false,
                type_arguments: None,
                arguments: Vec::new(),
            }),
        )
    }

    fn rewrite_call_arguments(&mut self, arguments: &[CallArgument]) -> Vec<CallArgument> {
        arguments
            .iter()
            .map(|argument| match argument {
                CallArgument::Expression(value) => {
                    CallArgument::Expression(Box::new(self.rewrite_expr(value)))
                }
                CallArgument::Spread(spread) => CallArgument::Spread(SpreadElement {
                    argument: Box::new(self.rewrite_expr(&spread.argument)),
                }),
                CallArgument::Missing(missing) => CallArgument::Missing(missing.clone()),
            })
            .collect()
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
            Expression::Class(class) => self.lower_class_expression(expression, class, None),
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
                let needs_safe_wrap = self.cjs_call_needs_safe_wrap(&call.callee);
                let callee = self.rewrite_expr(&call.callee);
                let callee = if needs_safe_wrap {
                    self.cjs_wrap_callee(callee)
                } else {
                    callee
                };
                let arguments = self.rewrite_call_arguments(&call.arguments);
                self.node(
                    expression.range(),
                    Expression::Call(CallExpression {
                        callee: Box::new(callee),
                        optional: call.optional,
                        type_arguments: None,
                        arguments,
                    }),
                )
            }
            Expression::New(new_expr) => {
                let callee = self.rewrite_expr(&new_expr.callee);
                let arguments = self.rewrite_call_arguments(&new_expr.arguments);
                self.node(
                    expression.range(),
                    Expression::New(NewExpression {
                        callee: Box::new(callee),
                        type_arguments: None,
                        arguments,
                    }),
                )
            }
            Expression::Member(member) => {
                let object = self.rewrite_expr(&member.object);
                let property = match &member.property {
                    MemberProperty::Computed(value) => {
                        MemberProperty::Computed(Box::new(self.rewrite_expr(value)))
                    }
                    other => other.clone(),
                };
                self.node(
                    expression.range(),
                    Expression::Member(MemberExpression {
                        object: Box::new(object),
                        property,
                        optional: member.optional,
                    }),
                )
            }
            Expression::Parenthesized(value) => {
                let value = self.rewrite_expr(value);
                self.node(
                    expression.range(),
                    Expression::Parenthesized(Box::new(value)),
                )
            }
            Expression::Sequence(value) => {
                let expressions = value
                    .expressions
                    .iter()
                    .map(|value| self.rewrite_expr(value))
                    .collect();
                self.node(
                    expression.range(),
                    Expression::Sequence(SequenceExpression { expressions }),
                )
            }
            Expression::Conditional(value) => {
                let test = self.rewrite_expr(&value.test);
                let consequent = self.rewrite_expr(&value.consequent);
                let alternate = self.rewrite_expr(&value.alternate);
                self.node(
                    expression.range(),
                    Expression::Conditional(ConditionalExpression {
                        test: Box::new(test),
                        consequent: Box::new(consequent),
                        alternate: Box::new(alternate),
                    }),
                )
            }
            Expression::Binary(value) => {
                let left = self.rewrite_expr(&value.left);
                let right = self.rewrite_expr(&value.right);
                if value.operator == BinaryOperator::Exponentiate
                    && Self::needs(LanguageFeature::Exponentiation, self.options)
                {
                    return self.math_pow(left, right, expression.range());
                }
                self.node(
                    expression.range(),
                    Expression::Binary(BinaryExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Logical(value) => {
                let left = self.rewrite_expr(&value.left);
                let right = self.rewrite_expr(&value.right);
                self.node(
                    expression.range(),
                    Expression::Logical(LogicalExpression {
                        operator: value.operator,
                        left: Box::new(left),
                        right: Box::new(right),
                    }),
                )
            }
            Expression::Unary(value) => {
                let argument = self.rewrite_expr(&value.argument);
                self.node(
                    expression.range(),
                    Expression::Unary(UnaryExpression {
                        operator: value.operator,
                        argument: Box::new(argument),
                    }),
                )
            }
            Expression::As(value) => {
                let inner = self.rewrite_expr(&value.expression);
                self.node(
                    expression.range(),
                    Expression::As(AsExpression {
                        expression: Box::new(inner),
                        type_node: value.type_node.clone(),
                    }),
                )
            }
            Expression::Satisfies(value) => {
                let inner = self.rewrite_expr(&value.expression);
                self.node(
                    expression.range(),
                    Expression::Satisfies(SatisfiesExpression {
                        expression: Box::new(inner),
                        type_node: value.type_node.clone(),
                    }),
                )
            }
            Expression::TypeAssertion(value) => {
                let inner = self.rewrite_expr(&value.expression);
                self.node(
                    expression.range(),
                    Expression::TypeAssertion(TypeAssertionExpression {
                        expression: Box::new(inner),
                        type_node: value.type_node.clone(),
                    }),
                )
            }
            Expression::NonNull(value) => {
                let inner = self.rewrite_expr(&value.expression);
                self.node(
                    expression.range(),
                    Expression::NonNull(NonNullExpression {
                        expression: Box::new(inner),
                    }),
                )
            }
            Expression::Import(value) => {
                let source = self.rewrite_expr(&value.source);
                let options = value
                    .options
                    .as_deref()
                    .map(|value| Box::new(self.rewrite_expr(value)));
                self.node(
                    expression.range(),
                    Expression::Import(ImportExpression {
                        source: Box::new(source),
                        options,
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
            Expression::Assignment(assignment) => {
                if assignment.operator == AssignmentOperator::ExponentiateAssign
                    && Self::needs(LanguageFeature::Exponentiation, self.options)
                {
                    return self.lower_compound_exponentiation(expression, assignment);
                }
                let right = self.rewrite_expr(&assignment.right);
                self.node(
                    expression.range(),
                    Expression::Assignment(AssignmentExpression {
                        right: Box::new(right),
                        ..assignment.clone()
                    }),
                )
            }
            Expression::Identifier(_) => {
                if let Some(replacement) = self.cjs_identifier_replacement(expression) {
                    replacement
                } else {
                    expression.clone()
                }
            }
            _ => expression.clone(),
        }
    }

    /// `Math.pow(base, exponent)` — the ES2016 exponentiation downlevel form.
    fn math_pow(&mut self, base: Expr, exponent: Expr, range: TextRange) -> Expr {
        let math = self.ident("Math");
        let callee = self.member_ident(&math, "pow", range);
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments: vec![
                    CallArgument::Expression(Box::new(base)),
                    CallArgument::Expression(Box::new(exponent)),
                ],
            }),
        )
    }

    fn coerce_property_key(&mut self, key: &Expr) -> Expr {
        let key = self.rewrite_expr(key);
        let range = key.range();
        let prop_key = self.helper_ident(HelperKind::PropKey);
        self.node(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(prop_key),
                optional: false,
                type_arguments: None,
                arguments: vec![CallArgument::Expression(Box::new(key))],
            }),
        )
    }

    /// Lowers `lhs **= rhs` below ES2016 to `lhs = Math.pow(lhs, rhs)`.
    /// Identifier targets read the binding directly. Member targets capture
    /// the base and every computed key so getters and right-hand expressions
    /// cannot redirect the assignment by mutating either value.
    fn lower_compound_exponentiation(
        &mut self,
        expression: &Expr,
        assignment: &AssignmentExpression,
    ) -> Expr {
        let right = self.rewrite_expr(&assignment.right);
        match assignment.left.data() {
            AssignmentTarget::Identifier(identifier) => {
                let read = self.node(
                    identifier.range(),
                    Expression::Identifier(identifier.clone()),
                );
                let lowered = Expression::Assignment(AssignmentExpression {
                    operator: AssignmentOperator::Assign,
                    left: assignment.left.clone(),
                    right: Box::new(self.math_pow(read, right, expression.range())),
                });
                self.node(expression.range(), lowered)
            }
            AssignmentTarget::Member(member) => {
                if self
                    .parameter_initializer_depths
                    .last()
                    .is_some_and(|depth| *depth == self.expression_temp_scopes.len())
                    && let MemberProperty::Computed(key) = &member.property
                    && !matches!(member.object.data(), Expression::Super)
                {
                    return self.lower_parameter_computed_exponentiation(
                        expression, assignment, member, key, right,
                    );
                }
                let object = self.rewrite_expr(&member.object);
                let (target_object, read_object) = if matches!(object.data(), Expression::Super) {
                    (object.clone(), object)
                } else {
                    let base = self.expression_temp();
                    let base_reference =
                        self.node(base.range(), Expression::Identifier(base.clone()));
                    let base_target = self.node(base.range(), AssignmentTarget::Identifier(base));
                    let base_assignment = self.node(
                        object.range(),
                        Expression::Assignment(AssignmentExpression {
                            operator: AssignmentOperator::Assign,
                            left: base_target,
                            right: Box::new(object),
                        }),
                    );
                    (base_assignment, base_reference)
                };
                let (target_property, read_property) = match &member.property {
                    MemberProperty::Computed(key) => {
                        let coerced_key = self.coerce_property_key(key);
                        let key_range = coerced_key.range();
                        let key_temp = self.expression_temp();
                        let key_reference =
                            self.node(key_temp.range(), Expression::Identifier(key_temp.clone()));
                        let key_target =
                            self.node(key_temp.range(), AssignmentTarget::Identifier(key_temp));
                        let key_assignment = self.node(
                            key_range,
                            Expression::Assignment(AssignmentExpression {
                                operator: AssignmentOperator::Assign,
                                left: key_target,
                                right: Box::new(coerced_key),
                            }),
                        );
                        (
                            MemberProperty::Computed(Box::new(key_assignment)),
                            MemberProperty::Computed(Box::new(key_reference)),
                        )
                    }
                    other => (other.clone(), other.clone()),
                };
                let read = self.member_expr(&read_object, read_property, expression.range());
                let target = self.node(
                    assignment.left.range(),
                    AssignmentTarget::Member(AssignmentMemberTarget {
                        object: Box::new(target_object),
                        property: target_property,
                    }),
                );
                let lowered = Expression::Assignment(AssignmentExpression {
                    operator: AssignmentOperator::Assign,
                    left: target,
                    right: Box::new(self.math_pow(read, right, expression.range())),
                });
                self.node(expression.range(), lowered)
            }
            AssignmentTarget::Object(_)
            | AssignmentTarget::Array(_)
            | AssignmentTarget::Missing(_) => expression.clone(),
        }
    }

    fn lower_parameter_computed_exponentiation(
        &mut self,
        expression: &Expr,
        assignment: &AssignmentExpression,
        member: &AssignmentMemberTarget,
        key: &Expr,
        right: Expr,
    ) -> Expr {
        let (target, read) = self.parameter_computed_reference(expression, assignment, member, key);
        let powered = self.math_pow(read, right, expression.range());
        self.node(
            expression.range(),
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left: target,
                right: Box::new(powered),
            }),
        )
    }

    fn parameter_computed_reference(
        &mut self,
        expression: &Expr,
        assignment: &AssignmentExpression,
        member: &AssignmentMemberTarget,
        key: &Expr,
    ) -> (AssignmentTargetNode, Expr) {
        let object = self.rewrite_expr(&member.object);
        let key = self.coerce_property_key(key);
        let frame = self.expression_temp();
        let frame_reference = self.node(frame.range(), Expression::Identifier(frame.clone()));
        let frame_target = self.node(frame.range(), AssignmentTarget::Identifier(frame));
        let frame_value = self.node(
            expression.range(),
            Expression::Array(ArrayLiteral {
                elements: vec![
                    ArrayElement::Expression(Box::new(object)),
                    ArrayElement::Expression(Box::new(key)),
                ],
            }),
        );
        // Assign only after key coercion returns. A recursive default initializer can then
        // overwrite the shared temp without corrupting this invocation's reference.
        let frame_assignment = self.node(
            expression.range(),
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left: frame_target,
                right: Box::new(frame_value),
            }),
        );
        let target = {
            let object = self.reference_frame_slot(&frame_assignment, "0", expression.range());
            let property = MemberProperty::Computed(Box::new(self.reference_frame_slot(
                &frame_reference,
                "1",
                expression.range(),
            )));
            self.node(
                assignment.left.range(),
                AssignmentTarget::Member(AssignmentMemberTarget {
                    object: Box::new(object),
                    property,
                }),
            )
        };
        let read = {
            let object = self.reference_frame_slot(&frame_reference, "0", expression.range());
            let property = MemberProperty::Computed(Box::new(self.reference_frame_slot(
                &frame_reference,
                "1",
                expression.range(),
            )));
            self.member_expr(&object, property, expression.range())
        };
        (target, read)
    }

    fn reference_frame_slot(&mut self, frame: &Expr, index: &str, range: TextRange) -> Expr {
        let index = self.number_expr(index);
        self.member_expr(frame, MemberProperty::Computed(Box::new(index)), range)
    }

    fn rewrite_arrow(&mut self, expression: &Expr, arrow: &ArrowFunction) -> Expr {
        let parameters = self.rewrite_parameters(&arrow.parameters);
        if arrow.is_async && Self::needs(LanguageFeature::AsyncFunctions, self.options) {
            let previous = self.replace_await;
            self.replace_await = true;
            let body = match &arrow.body {
                FunctionBody::Expression(expr) => {
                    self.expression_temp_scopes.push(Vec::new());
                    let rewritten = self.rewrite_expr(expr);
                    let temps = self.expression_temp_scopes.pop().unwrap_or_default();
                    let mut statements = Vec::new();
                    if let Some(declaration) = self.temp_declaration(temps, expression.range()) {
                        statements.push(declaration);
                    }
                    statements.push(self.node(
                        expression.range(),
                        Statement::Return(ReturnStatement {
                            argument: Some(Box::new(rewritten)),
                        }),
                    ));
                    let block = self.node(expression.range(), Block { statements });
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
                    parameters,
                    return_type: None,
                    body: FunctionBody::Expression(Box::new(call)),
                }),
            );
        }
        let body = match &arrow.body {
            FunctionBody::Expression(expr) => {
                self.expression_temp_scopes.push(Vec::new());
                let rewritten = self.rewrite_expr(expr);
                let temps = self.expression_temp_scopes.pop().unwrap_or_default();
                match self.temp_declaration(temps, expression.range()) {
                    None => FunctionBody::Expression(Box::new(rewritten)),
                    Some(declaration) => {
                        let returned = self.node(
                            expression.range(),
                            Statement::Return(ReturnStatement {
                                argument: Some(Box::new(rewritten)),
                            }),
                        );
                        FunctionBody::Block(self.node(
                            expression.range(),
                            Block {
                                statements: vec![declaration, returned],
                            },
                        ))
                    }
                }
            }
            other => self.rewrite_body(other),
        };
        self.node(
            expression.range(),
            Expression::Arrow(ArrowFunction {
                parameters,
                body,
                ..arrow.clone()
            }),
        )
    }
}

fn is_super_call_expression(expression: &Expr) -> bool {
    let Expression::Call(call) = expression.data() else {
        return false;
    };
    matches!(call.callee.data(), Expression::Super)
}

/// Converts a UTF-16 offset into a byte offset for plain-source text.
fn utf16_to_byte(text: &str, offset: usize) -> Option<usize> {
    if offset == 0 {
        return Some(0);
    }
    let mut units = 0usize;
    for (index, character) in text.char_indices() {
        if units == offset {
            return Some(index);
        }
        units += character.len_utf16();
    }
    if units == offset {
        return Some(text.len());
    }
    None
}

/// Raw token text taken straight from the source file.
fn raw_token_text(file: &SourceFile, token: &Token) -> String {
    let start = file
        .source_text()
        .utf16_to_byte(token.range().start())
        .unwrap_or(0);
    let end = file
        .source_text()
        .utf16_to_byte(token.range().end())
        .unwrap_or(0);
    file.source_text()
        .as_str()
        .get(start..end)
        .unwrap_or("")
        .to_owned()
}

/// The unescaped identifier spelling for a source identifier node.
fn identifier_text(file: &SourceFile, ident: &IdentifierNode) -> String {
    raw_token_text(file, ident.data().token())
}

/// Declared value names of a statement, for export assignment generation.
fn declared_binding_names(file: &SourceFile, statement: &Stmt) -> Vec<String> {
    match statement.data() {
        Statement::Variable(declaration) => declaration
            .declarations
            .iter()
            .filter_map(|declarator| match declarator.data().binding.data() {
                BindingPattern::Identifier(ident) => Some(identifier_text(file, ident)),
                _ => None,
            })
            .collect(),
        Statement::Enum(declaration) => vec![raw_token_text(file, declaration.name.data().token())],
        _ => Vec::new(),
    }
}

fn property_key_text(rewriter: &mut Rewriter, property: &ObjectBindingProperty) -> Option<String> {
    match &property.name {
        PropertyName::Identifier(ident) => {
            Some(rewriter.token_text(ident.data().token()).to_owned())
        }
        PropertyName::String(literal) => {
            let raw = rewriter.token_text(literal.data().token());
            Some(raw.trim_matches(['"', '\'']).to_owned())
        }
        _ => None,
    }
}

/// Source range of a property name, taken from its inner node.
fn property_name_range(name: &PropertyName) -> TextRange {
    match name {
        PropertyName::Identifier(ident) => ident.range(),
        PropertyName::String(literal) => literal.range(),
        PropertyName::Number(number) => number.range(),
        PropertyName::Computed(expression) => expression.range(),
        PropertyName::Private(private) => private.range(),
        PropertyName::Missing(_) => {
            TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO).expect("zero-width range is ordered")
        }
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
                    if let Some(initializer) = declarator.data().initializer.as_deref() {
                        scan_expression(initializer, features);
                    }
                }
            }
            Statement::Expression(expr) => scan_expression(&expr.expression, features),
            Statement::If(value) => {
                scan_expression(&value.test, features);
                scan_nested_statements(std::slice::from_ref(value.consequent.as_ref()), features);
                if let Some(alternate) = value.alternate.as_deref() {
                    scan_nested_statements(std::slice::from_ref(alternate), features);
                }
            }
            Statement::While(value) => {
                scan_expression(&value.test, features);
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::DoWhile(value) => {
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
                scan_expression(&value.test, features);
            }
            Statement::With(value) => {
                scan_expression(&value.object, features);
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::Labeled(value) => {
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::Return(ret) => {
                if let Some(argument) = ret.argument.as_deref() {
                    scan_expression(argument, features);
                }
            }
            Statement::Throw(value) => scan_expression(&value.argument, features),
            Statement::For(value) => {
                if let Some(ForInitializer::Expression(initializer)) = &value.initializer {
                    scan_expression(initializer, features);
                }
                if let Some(test) = value.test.as_deref() {
                    scan_expression(test, features);
                }
                if let Some(update) = value.update.as_deref() {
                    scan_expression(update, features);
                }
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::ForIn(for_in) => {
                scan_expression(&for_in.object, features);
                scan_nested_statements(std::slice::from_ref(for_in.body.as_ref()), features);
            }
            Statement::ForOf(for_of) => {
                if for_of.mode == ForOfMode::Async {
                    features.insert(LanguageFeature::AsyncIteration);
                }
                scan_expression(&for_of.iterable, features);
                scan_nested_statements(std::slice::from_ref(for_of.body.as_ref()), features);
            }
            Statement::Switch(value) => {
                scan_expression(&value.discriminant, features);
                for case in &value.cases {
                    if let Some(test) = case.data().test.as_deref() {
                        scan_expression(test, features);
                    }
                    scan_nested_statements(&case.data().consequent, features);
                }
            }
            Statement::Try(value) => {
                scan_nested_statements(&value.block.data().statements, features);
                if let Some(handler) = &value.handler {
                    scan_nested_statements(&handler.data().body.data().statements, features);
                }
                if let Some(finalizer) = &value.finalizer {
                    scan_nested_statements(&finalizer.data().statements, features);
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
            Statement::Export(ExportDeclaration::Default(default)) => {
                if let ExportDefaultValue::Class(class) = &default.value {
                    features.insert(LanguageFeature::Classes);
                    scan_class(file, class, options, features, diagnostics);
                } else if let ExportDefaultValue::Expression(value) = &default.value {
                    scan_expression(value, features);
                }
            }
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
    if let Some(heritage) = &class.extends {
        scan_expression(&heritage.expression, features);
    }
    for member in &class.members {
        match member.data() {
            ClassMember::Property(property) => {
                if !property.modifiers.is_declare && !property.modifiers.is_abstract {
                    features.insert(LanguageFeature::ClassFields);
                }
                scan_computed_name(&property.name, features);
                if let Some(initializer) = property.initializer.as_deref() {
                    scan_expression(initializer, features);
                }
                if matches!(property.name, PropertyName::Private(_))
                    && !options.target.supports(LanguageFeature::ClassFields)
                {
                    diagnostics.push(Diagnostic::error(
                        codes::PRIVATE_FIELD_REQUIRES_ES2022,
                        file.source_id(),
                        member.range(),
                        "private class fields require ScriptTarget::Es2022 or later",
                    ));
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
            ClassMember::AutoAccessor(accessor) => {
                features.insert(LanguageFeature::ClassFields);
                scan_computed_name(&accessor.name, features);
                if let Some(initializer) = accessor.initializer.as_deref() {
                    scan_expression(initializer, features);
                }
                if !options.target.supports(LanguageFeature::ClassFields) {
                    diagnostics.push(Diagnostic::error(
                        codes::AUTO_ACCESSOR_REQUIRES_ES2022,
                        file.source_id(),
                        member.range(),
                        "auto-accessors require ScriptTarget::Es2022 or later",
                    ));
                }
            }
            ClassMember::StaticBlock(block) => {
                features.insert(LanguageFeature::ClassFields);
                scan_statements(
                    file,
                    &block.data().statements,
                    options,
                    features,
                    diagnostics,
                    false,
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
        if let Some(initializer) = parameter.data().initializer.as_deref() {
            scan_expression(initializer, features);
        }
    }
    match function.body.as_ref() {
        Some(FunctionBody::Block(body)) => scan_statements(
            file,
            &body.data().statements,
            options,
            features,
            diagnostics,
            function.is_async,
        ),
        Some(FunctionBody::Expression(expression)) => scan_expression(expression, features),
        Some(FunctionBody::Missing(_)) | None => {}
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

/// Recursively scans one expression for exponentiation syntax. The deep walk
/// intentionally reports only [`LanguageFeature::Exponentiation`]: every other
/// feature keeps the statement-level analysis surface this module has always
/// exposed, and only exponentiation is lowered in every expression position
/// the rewriter visits.
fn scan_expression(expression: &Expr, features: &mut BTreeSet<LanguageFeature>) {
    match expression.data() {
        Expression::Binary(value) => {
            scan_expression(&value.left, features);
            if value.operator == BinaryOperator::Exponentiate {
                features.insert(LanguageFeature::Exponentiation);
            }
            scan_expression(&value.right, features);
        }
        Expression::Assignment(value) => {
            scan_target(value.left.data(), features);
            if value.operator == AssignmentOperator::ExponentiateAssign {
                features.insert(LanguageFeature::Exponentiation);
            }
            scan_expression(&value.right, features);
        }
        Expression::Template(value) => {
            for expression in &value.expressions {
                scan_expression(expression, features);
            }
        }
        Expression::TaggedTemplate(value) => {
            scan_expression(&value.tag, features);
            for expression in &value.template.expressions {
                scan_expression(expression, features);
            }
        }
        Expression::Array(value) => {
            for element in &value.elements {
                if let ArrayElement::Expression(expression) = element {
                    scan_expression(expression, features);
                }
            }
        }
        Expression::Object(value) => {
            for member in &value.members {
                match member.data() {
                    ObjectMember::Property(property) => {
                        scan_computed_name(&property.name, features);
                        scan_expression(&property.value, features);
                    }
                    ObjectMember::Method(method) => {
                        scan_computed_name(&method.name, features);
                        scan_parameter_defaults(&method.function.parameters, features);
                        scan_nested_body(method.function.body.as_ref(), features);
                    }
                    ObjectMember::Spread(spread) => scan_expression(&spread.argument, features),
                    ObjectMember::Missing(_) => {}
                }
            }
        }
        Expression::Function(value) => {
            scan_parameter_defaults(&value.function.parameters, features);
            scan_nested_body(value.function.body.as_ref(), features);
        }
        Expression::Class(value) => scan_class_expressions(&value.class, features),
        Expression::Arrow(value) => {
            scan_parameter_defaults(&value.parameters, features);
            scan_nested_body(Some(&value.body), features);
        }
        Expression::Call(value) => {
            scan_expression(&value.callee, features);
            for argument in &value.arguments {
                match argument {
                    CallArgument::Expression(expression) => scan_expression(expression, features),
                    CallArgument::Spread(spread) => scan_expression(&spread.argument, features),
                    CallArgument::Missing(_) => {}
                }
            }
        }
        Expression::Member(value) => {
            scan_expression(&value.object, features);
            if let MemberProperty::Computed(key) = &value.property {
                scan_expression(key, features);
            }
        }
        Expression::New(value) => {
            scan_expression(&value.callee, features);
            for argument in &value.arguments {
                match argument {
                    CallArgument::Expression(expression) => scan_expression(expression, features),
                    CallArgument::Spread(spread) => scan_expression(&spread.argument, features),
                    CallArgument::Missing(_) => {}
                }
            }
        }
        Expression::Await(value) => scan_expression(&value.argument, features),
        Expression::Yield(value) => {
            if let Some(argument) = value.argument.as_deref() {
                scan_expression(argument, features);
            }
        }
        Expression::Unary(value) => scan_expression(&value.argument, features),
        Expression::Update(value) => scan_target(value.argument.data(), features),
        Expression::Logical(value) => {
            scan_expression(&value.left, features);
            scan_expression(&value.right, features);
        }
        Expression::Conditional(value) => {
            scan_expression(&value.test, features);
            scan_expression(&value.consequent, features);
            scan_expression(&value.alternate, features);
        }
        Expression::Sequence(value) => {
            for expression in &value.expressions {
                scan_expression(expression, features);
            }
        }
        Expression::Parenthesized(value) => scan_expression(value, features),
        Expression::As(value) => scan_expression(&value.expression, features),
        Expression::Satisfies(value) => scan_expression(&value.expression, features),
        Expression::TypeAssertion(value) => scan_expression(&value.expression, features),
        Expression::NonNull(value) => scan_expression(&value.expression, features),
        Expression::Import(value) => {
            scan_expression(&value.source, features);
            if let Some(options) = value.options.as_deref() {
                scan_expression(options, features);
            }
        }
        // JSX trees print through the JSX desugar plan, which owns their
        // expression containers, so the transform walk never visits them.
        _ => {}
    }
}

/// Recursively scans an assignment target for exponentiation syntax.
fn scan_target(target: &AssignmentTarget, features: &mut BTreeSet<LanguageFeature>) {
    match target {
        AssignmentTarget::Member(member) => {
            scan_expression(&member.object, features);
            if let MemberProperty::Computed(key) = &member.property {
                scan_expression(key, features);
            }
        }
        AssignmentTarget::Object(pattern) => {
            for property in &pattern.properties {
                scan_computed_name(&property.name, features);
                scan_target(property.target.data(), features);
                if let Some(initializer) = property.initializer.as_deref() {
                    scan_expression(initializer, features);
                }
            }
        }
        AssignmentTarget::Array(pattern) => {
            for element in &pattern.elements {
                if let AssignmentArrayElement::Target(inner) = element {
                    scan_target(inner.data(), features);
                }
            }
        }
        AssignmentTarget::Identifier(_) | AssignmentTarget::Missing(_) => {}
    }
}

/// Scans a property name for exponentiation inside a computed key.
fn scan_computed_name(name: &PropertyName, features: &mut BTreeSet<LanguageFeature>) {
    if let PropertyName::Computed(key) = name {
        scan_expression(key, features);
    }
}

/// Scans parameter default values for exponentiation syntax.
fn scan_parameter_defaults(parameters: &[ParameterNode], features: &mut BTreeSet<LanguageFeature>) {
    for parameter in parameters {
        if let Some(initializer) = parameter.data().initializer.as_deref() {
            scan_expression(initializer, features);
        }
    }
}

/// Scans a nested function body for exponentiation syntax.
fn scan_nested_body(body: Option<&FunctionBody>, features: &mut BTreeSet<LanguageFeature>) {
    match body {
        Some(FunctionBody::Block(block)) => {
            scan_nested_statements(&block.data().statements, features);
        }
        Some(FunctionBody::Expression(expression)) => scan_expression(expression, features),
        Some(FunctionBody::Missing(_)) | None => {}
    }
}

/// Exponentiation-only statement walk for bodies the rewriter descends into
/// from expression position (nested functions, arrows, object methods).
fn scan_nested_statements(statements: &[Stmt], features: &mut BTreeSet<LanguageFeature>) {
    for statement in statements {
        match statement.data() {
            Statement::Variable(declaration) => {
                for declarator in &declaration.declarations {
                    if let Some(initializer) = declarator.data().initializer.as_deref() {
                        scan_expression(initializer, features);
                    }
                }
            }
            Statement::Expression(expr) => scan_expression(&expr.expression, features),
            Statement::Return(ret) => {
                if let Some(argument) = ret.argument.as_deref() {
                    scan_expression(argument, features);
                }
            }
            Statement::Throw(value) => scan_expression(&value.argument, features),
            Statement::If(value) => {
                scan_expression(&value.test, features);
                scan_nested_statements(std::slice::from_ref(value.consequent.as_ref()), features);
                if let Some(alternate) = value.alternate.as_deref() {
                    scan_nested_statements(std::slice::from_ref(alternate), features);
                }
            }
            Statement::While(value) => {
                scan_expression(&value.test, features);
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::DoWhile(value) => {
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
                scan_expression(&value.test, features);
            }
            Statement::With(value) => {
                scan_expression(&value.object, features);
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::Labeled(value) => {
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::For(value) => {
                if let Some(ForInitializer::Expression(initializer)) = &value.initializer {
                    scan_expression(initializer, features);
                }
                if let Some(test) = value.test.as_deref() {
                    scan_expression(test, features);
                }
                if let Some(update) = value.update.as_deref() {
                    scan_expression(update, features);
                }
                scan_nested_statements(std::slice::from_ref(value.body.as_ref()), features);
            }
            Statement::ForIn(for_in) => {
                scan_expression(&for_in.object, features);
                scan_nested_statements(std::slice::from_ref(for_in.body.as_ref()), features);
            }
            Statement::ForOf(for_of) => {
                scan_expression(&for_of.iterable, features);
                scan_nested_statements(std::slice::from_ref(for_of.body.as_ref()), features);
            }
            Statement::Switch(value) => {
                scan_expression(&value.discriminant, features);
                for case in &value.cases {
                    if let Some(test) = case.data().test.as_deref() {
                        scan_expression(test, features);
                    }
                    scan_nested_statements(&case.data().consequent, features);
                }
            }
            Statement::Try(value) => {
                scan_nested_statements(&value.block.data().statements, features);
                if let Some(handler) = &value.handler {
                    scan_nested_statements(&handler.data().body.data().statements, features);
                }
                if let Some(finalizer) = &value.finalizer {
                    scan_nested_statements(&finalizer.data().statements, features);
                }
            }
            Statement::Function(function) => {
                scan_parameter_defaults(&function.function.parameters, features);
                scan_nested_body(function.function.body.as_ref(), features);
            }
            Statement::Class(class) => scan_class_expressions(class, features),
            Statement::Block(block) => {
                scan_nested_statements(&block.data().statements, features);
            }
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                inner,
            ))) => scan_nested_statements(std::slice::from_ref(inner), features),
            Statement::Export(ExportDeclaration::Default(default)) => {
                if let ExportDefaultValue::Expression(value) = &default.value {
                    scan_expression(value, features);
                }
            }
            _ => {}
        }
    }
}

/// Exponentiation-only walk over the expression positions of one class.
fn scan_class_expressions(class: &ClassDeclaration, features: &mut BTreeSet<LanguageFeature>) {
    if let Some(heritage) = &class.extends {
        scan_expression(&heritage.expression, features);
    }
    for member in &class.members {
        match member.data() {
            ClassMember::Property(property) => {
                scan_computed_name(&property.name, features);
                if let Some(initializer) = property.initializer.as_deref() {
                    scan_expression(initializer, features);
                }
            }
            ClassMember::Method(method) => {
                scan_computed_name(&method.name, features);
                scan_parameter_defaults(&method.function.parameters, features);
                scan_nested_body(method.function.body.as_ref(), features);
            }
            ClassMember::AutoAccessor(accessor) => {
                scan_computed_name(&accessor.name, features);
                if let Some(initializer) = accessor.initializer.as_deref() {
                    scan_expression(initializer, features);
                }
            }
            ClassMember::StaticBlock(block) => {
                scan_nested_statements(&block.data().statements, features);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LanguageFeature, ScriptTarget, analyze, codes, emit_transformed};
    use crate::checker;
    use crate::diagnostic::Recovered;
    use crate::emitter::{EmitFileNames, EmitOptions, EmitOutput, ModuleKind};
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
            ..EmitFileNames::default()
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

    fn emit_with_options(source: &str, options: EmitOptions) -> EmitOutput {
        let file = parse(source);
        let checked = checker::check(&Recovered::clean(parse(source)));
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
    fn uninitialized_public_fields_create_own_undefined_properties() {
        let output = emit_at("class C { x; static y; }\n", ScriptTarget::Es2015);
        let code = javascript(&output);
        assert!(code.contains("this.x = void 0"), "{code}");
        assert!(code.contains("C.y = void 0"), "{code}");
        assert!(!code.lines().any(|line| line.trim() == "x;"), "{code}");
    }

    #[test]
    fn computed_field_names_are_evaluated_once_at_class_definition() {
        let output = emit_at(
            "class C { [key()] = 1; } new C(); new C();\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert_eq!(code.matches("key()").count(), 1, "{code}");
        assert!(code.contains("let _t0 = key()"), "{code}");
        assert!(code.contains("this[_t0] = 1"), "{code}");
    }
    #[test]
    fn computed_keys_preserve_heritage_and_member_evaluation_order() {
        let output = emit_at(
            "class C extends heritage() { [field()] = 1; [method()]() {} [later()] = 2; }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let heritage = code.find("heritage()").expect("heritage evaluation");
        let field = code.find("field()").expect("first computed field");
        let method = code.find("method()").expect("computed method");
        let later = code.find("later()").expect("later computed field");
        let class = code.find("class C extends _t0").expect("rewritten class");
        assert!(
            heritage < field && field < method && method < later && later < class,
            "{code}"
        );
        assert!(code.contains("this[_t1] = 1"), "{code}");
        assert!(code.contains("[_t2]()"), "{code}");
        assert!(code.contains("this[_t3] = 2"), "{code}");
    }
    #[test]
    fn static_computed_field_runs_key_and_value_and_assigns_the_property() {
        let output = emit_at(
            "class C { static [key()] = value(); }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert_eq!(code.matches("key()").count(), 1, "{code}");
        assert_eq!(code.matches("value()").count(), 1, "{code}");
        let key = code.find("let _t0 = key()").expect("key setup");
        let class = code.find("class C").expect("class declaration");
        let assignment = code
            .find("C[_t0] = value()")
            .expect("post-class static property assignment");
        assert!(key < class && class < assignment, "{code}");
    }
    #[test]
    fn named_export_wraps_only_the_lowered_class() {
        let output = emit_at(
            "export class C { static [key()] = value(); }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let prelude = code.find("let _t0 = key()").expect("ordinary key prelude");
        let class = code
            .find("export class C")
            .expect("exported rewritten class");
        let postlude = code
            .find("C[_t0] = value()")
            .expect("ordinary static postlude");
        assert!(prelude < class && class < postlude, "{code}");
        assert!(!code.contains("export let _t0"), "{code}");
        assert!(!code.contains("export C[_t0]"), "{code}");
    }

    #[test]
    fn named_default_export_keeps_class_and_generated_statements_separate() {
        let output = emit_at(
            "export default class C { field = 1; static [key()] = value(); }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let prelude = code.find("let _t0 = key()").expect("ordinary key prelude");
        let class = code
            .find("export default class C")
            .expect("named default class");
        let postlude = code
            .find("C[_t0] = value()")
            .expect("ordinary static postlude");
        assert!(prelude < class && class < postlude, "{code}");
        assert!(code.contains("this.field = 1"), "{code}");
        assert_eq!(code.matches("key()").count(), 1, "{code}");
        assert_eq!(code.matches("value()").count(), 1, "{code}");
        assert!(!code.contains("export default let"), "{code}");
    }

    #[test]
    fn anonymous_default_export_lowers_instance_fields_in_place() {
        let output = emit_at(
            "export default class { field = 1; }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("export default class"), "{code}");
        assert!(code.contains("constructor"), "{code}");
        assert!(code.contains("this.field = 1"), "{code}");
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code() == codes::STATIC_FIELD_REQUIRES_CLASS_NAME }),
            "{:?}",
            output.diagnostics
        );
    }

    #[test]
    fn anonymous_default_static_field_uses_one_synthetic_class_target() {
        let output = emit_at(
            "export default class { static [key()] = value(); }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let prelude = code.find("let _t1 = key()").expect("ordinary key prelude");
        let class = code
            .find("export default class _t0")
            .expect("synthetically named default class");
        let postlude = code
            .find("_t0[_t1] = value()")
            .expect("static assignment uses synthetic class name");
        assert!(prelude < class && class < postlude, "{code}");
        assert_eq!(code.matches("key()").count(), 1, "{code}");
        assert_eq!(code.matches("value()").count(), 1, "{code}");
    }
    #[test]
    fn anonymous_default_static_class_restores_imported_name_to_default() {
        let output = emit_at(
            "export default class { static x = 1; }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let class = code
            .find("export default class _t0")
            .expect("synthetically named default class");
        let restore = code
            .find(r#"Object.defineProperty(_t0, "name""#)
            .expect("default name restoration");
        let static_assignment = code
            .find("_t0.x = 1")
            .expect("post-class static assignment");
        assert!(class < restore && restore < static_assignment, "{code}");
        assert!(code.contains(r#"value: "default""#), "{code}");
        assert!(code.contains("configurable: true"), "{code}");
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
    fn static_fields_and_blocks_remain_interleaved() {
        let output = emit_at(
            "class C { static a = one(); static { step(); } static b = two(); }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let first = code.find("C.a = one()").expect("first static field");
        let block = code.find("step()").expect("static block");
        let second = code.find("C.b = two()").expect("second static field");
        assert!(first < block && block < second, "{code}");
        assert!(!code.contains("static {"), "{code}");
    }

    #[test]
    fn define_mode_uses_complete_data_descriptors() {
        let output = emit_with_options(
            "class C { x = 1; static y = 2; }\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                use_define_for_class_fields: Some(true),
                ..EmitOptions::default()
            },
        );
        let code = javascript(&output);
        assert!(code.contains("Object.defineProperty(this, \"x\""), "{code}");
        assert!(code.contains("Object.defineProperty(C, \"y\""), "{code}");
        let enumerable = code
            .find("enumerable: true")
            .expect("enumerable descriptor");
        let configurable = code
            .find("configurable: true")
            .expect("configurable descriptor");
        let writable = code.find("writable: true").expect("writable descriptor");
        let value = code.find("value: 1").expect("descriptor value");
        assert!(
            enumerable < configurable && configurable < writable && writable < value,
            "{code}"
        );
    }

    #[test]
    fn declare_and_abstract_fields_have_no_runtime_initializers() {
        let output = emit_at(
            "abstract class C { declare x: number; abstract y: string; z = 1; }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(!code.contains("this.x"), "{code}");
        assert!(!code.contains("this.y"), "{code}");
        assert!(code.contains("this.z = 1"), "{code}");
    }

    #[test]
    fn nested_super_sites_receive_fields_after_each_completion() {
        let output = emit_at(
            "class C extends B { x = 1; constructor(flag) { if (flag) super(1); else { super(2); } } }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert_eq!(code.matches("this.x = 1").count(), 2, "{code}");
        let first_super = code.find("super(1)").expect("first super");
        let first_init = code.find("this.x = 1").expect("first init");
        assert!(first_super < first_init, "{code}");
    }

    #[test]
    fn class_expressions_lower_inside_call_arguments() {
        let output = emit_at(
            "consume(class { static x = 1; y = 2; });\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("consume((() =>"), "{code}");
        assert!(code.contains("this.y = 2"), "{code}");
        assert!(code.contains(".x = 1"), "{code}");
        assert!(!code.contains("static x"), "{code}");
    }

    #[test]
    fn static_this_and_super_are_rebound_to_the_constructor() {
        let output = emit_at(
            "class B { static p() {} } class C extends B { static x = this; static { super.p(); } }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("C.x = C"), "{code}");
        assert!(
            code.contains("Reflect.get(Object.getPrototypeOf(C), \"p\", C).call(C)"),
            "{code}"
        );
    }

    #[test]
    fn static_block_only_source_is_detected_as_class_fields() {
        let file = parse("class C { static { run(); } }\n");
        let options = options_for(ScriptTarget::Es2015).transform_view();
        let plan = analyze(&file, &options);
        assert!(plan.features.contains(&LanguageFeature::ClassFields));
        assert!(plan.required.contains(&LanguageFeature::ClassFields));
    }
    #[test]
    fn nested_static_blocks_rewrite_this_through_control_flow() {
        let output = emit_at(
            "class C { static { if (flag) { for (; once(); ) { this.x = this; break; } } try { throw this; } catch (e) { this.y = e; } finally { this.z = this; } } }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("C.x = C"), "{code}");
        assert!(code.contains("throw C"), "{code}");
        assert!(code.contains("C.y = e"), "{code}");
        assert!(code.contains("C.z = C"), "{code}");
    }

    #[test]
    fn static_super_writes_use_reflect_with_single_evaluation() {
        let output = emit_at(
            "class B { static get x() { return 1; } static set x(v) {} } class C extends B { static { super.x = rhs(); super[key()] += more(); } }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(!code.contains("super.x ="), "{code}");
        assert!(!code.contains("super[key()]"), "{code}");
        assert_eq!(code.matches("key()").count(), 1, "{code}");
        assert_eq!(code.matches("rhs()").count(), 1, "{code}");
        assert_eq!(code.matches("more()").count(), 1, "{code}");
        assert!(code.matches("Reflect.set(").count() >= 2, "{code}");
        assert!(code.contains("Reflect.get("), "{code}");
    }

    #[test]
    fn node_executes_nested_static_super_writes() {
        let source = r#"
const log = [];
function key() { log.push("key"); return "x"; }
function rhs() { log.push("rhs"); return 3; }
class B {
  static get x() { log.push("get"); return this._x ?? 1; }
  static set x(value) { log.push("set"); this._x = value; }
}
class C extends B {
  static {
    if (true) {
      super.x = 2;
      try { super[key()] += rhs(); this.self = this; }
      finally { this.done = true; }
    }
  }
}
console.log(JSON.stringify([C._x, log, C.self === C, C.done]));
"#;
        let output = emit_at(source, ScriptTarget::Es2015);
        let code = javascript(&output);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bamts-static-super-{nonce}.mjs"));
        std::fs::write(&path, code).expect("write lowered JavaScript");
        let result = std::process::Command::new("node")
            .arg(&path)
            .output()
            .expect("execute Node");
        let _ = std::fs::remove_file(&path);
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout).expect("Node stdout"),
            "[5,[\"set\",\"key\",\"get\",\"rhs\",\"set\"],true,true]\n"
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
    fn strict_prologue_follows_always_strict_and_module_kind() {
        // A top-level external module takes the prologue exactly when it is
        // transpiled to CommonJS; under ES module output it takes none even
        // with alwaysStrict on (673 baselines). Scripts follow alwaysStrict
        // alone, as conformance pairs like arrowFunctionContexts(alwaysstrict=...)
        // show.
        let module = emit_with_options(
            "export const value = 1;\n",
            EmitOptions {
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        assert!(
            javascript(&module).starts_with("\"use strict\";\n"),
            "a CommonJS external module implies strict: {}",
            javascript(&module)
        );
        let es_module = emit_with_options(
            "export const value = 1;\n",
            EmitOptions {
                module: Some(crate::emitter::ModuleKind::Es2015),
                always_strict: true,
                ..EmitOptions::default()
            },
        );
        assert!(
            !javascript(&es_module).starts_with("\"use strict\";\n"),
            "an ES external module takes no prologue even under alwaysStrict: {}",
            javascript(&es_module)
        );
        let commonjs_script = emit_with_options(
            "const value = 1;\n",
            EmitOptions {
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        assert!(
            !javascript(&commonjs_script).starts_with("\"use strict\";\n"),
            "a script under CommonJS still follows alwaysStrict: {}",
            javascript(&commonjs_script)
        );
        let script = emit_with_options(
            "const value = 1;\n",
            EmitOptions {
                ..EmitOptions::default()
            },
        );
        assert!(
            !javascript(&script).starts_with("\"use strict\";\n"),
            "a plain script has no prologue: {}",
            javascript(&script)
        );
        let always = emit_with_options(
            "const value = 1;\n",
            EmitOptions {
                always_strict: true,
                ..EmitOptions::default()
            },
        );
        assert!(
            javascript(&always).starts_with("\"use strict\";\n"),
            "alwaysStrict emits the prologue: {}",
            javascript(&always)
        );
        let deduped = emit_with_options(
            "\"use strict\";\nconst value = 1;\n",
            EmitOptions {
                always_strict: true,
                ..EmitOptions::default()
            },
        );
        let code = javascript(&deduped);
        assert_eq!(
            code.matches("\"use strict\";").count(),
            1,
            "a source directive suppresses the synthetic one: {code}"
        );
    }

    #[test]
    fn ambient_enum_and_module_produce_no_runtime_statements() {
        // Upstream ambientEnumElementInitializer5 (43527a9c) emits only the
        // strict prologue; ambientModules (df2119af) keeps only the trailing
        // member assignment.
        let ambient_enum = emit_at("declare enum E { e = -0xA }\n", ScriptTarget::Es2015);
        assert_eq!(
            javascript(&ambient_enum),
            "",
            "{:?}",
            javascript(&ambient_enum)
        );

        let ambient_module = emit_at(
            "declare namespace Foo.Bar { export var foo; };\nFoo.Bar.foo = 5;\n",
            ScriptTarget::Es2015,
        );
        // The ambient declaration vanishes; the runtime assignment survives,
        // matching upstream ambientModules (df2119af) after the prologue.
        assert!(
            !javascript(&ambient_module).starts_with("declare")
                && !javascript(&ambient_module).contains("export var foo"),
            "the ambient block is erased: {}",
            javascript(&ambient_module)
        );

        let runtime = emit_at("const enum K { A }\nlet kept = 1;\n", ScriptTarget::Es2015);
        assert!(
            javascript(&runtime).contains("kept"),
            "non-ambient code survives: {}",
            javascript(&runtime)
        );
    }

    #[test]
    fn generic_call_type_arguments_are_erased() {
        // Upstream genericCallWithNonGenericArgs1 (e71ae63c): f<any>(null)
        // emits f(null);
        let output = emit_at(
            "function f<T>(x: any) { }\nf<any>(null);\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("f(null);"), "{code}");
        assert!(!code.contains("<any>"), "{code}");
    }

    #[test]
    fn commonjs_named_import_and_function_export_match_upstream_shape() {
        // Upstream commonjsSafeImport(target=es2015).js
        // (282283386bc4040f0eba585ac33e142c5c30cfbb67f969b1706c655dc7bee408).
        let export = emit_with_options(
            "export function Foo() {}\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        let export_code = javascript(&export);
        assert!(
            export_code.starts_with(
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.Foo = Foo;\nfunction Foo()"
            ),
            "{export_code}"
        );

        let import = emit_with_options(
            "import { Foo } from './10_lib';\nFoo();\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        let import_code = javascript(&import);
        assert!(
            import_code.starts_with(
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nconst _10_lib_1 = require(\"./10_lib\");\n"
            ),
            "{import_code}"
        );
        assert!(
            import_code.contains("(0, _10_lib_1.Foo)();"),
            "{import_code}"
        );
        assert!(!import_code.contains("import "), "{import_code}");
    }

    #[test]
    fn es5_lowers_nested_computed_destructuring_assignment() {
        // Upstream computedPropertiesInDestructuring1.js
        // (50845e031ebf8203c61d4bfb72d37eea703fea245df4e215c7b5772583a8a898).
        let output = emit_at(
            "let foo = 'bar', bar4; [{ [foo]: bar4 }] = [{ bar: 'bar' }];\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code()
                    == codes::DESTRUCTURING_ASSIGNMENT_REQUIRES_ES2015),
            "{:?}",
            output.diagnostics
        );
        assert!(code.contains("bar4 ="), "{code}");
        assert!(code.contains("[foo]"), "{code}");
        assert!(!code.contains("[{ [foo]: bar4 }] ="), "{code}");
    }

    #[test]
    fn commonjs_class_and_variable_exports_receive_void_0_preamble() {
        // Upstream importHelpersWithImportStarAs a.js (a2836e68) emits
        // `exports.A = void 0;` + class + `exports.A = A;`; the variable form
        // follows es6ImportDefaultBindingWithExport client.js (adfd4388).
        let output = emit_with_options(
            "export class A {}\nexport const x = 1;\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        let code = javascript(&output);
        assert!(
            code.starts_with(
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.A = exports.x = void 0;\n"
            ),
            "{code}"
        );
        let marker = code
            .find("exports.A = exports.x = void 0;")
            .expect("preamble");
        let class_at = code.find("class A").expect("class kept");
        let class_export = code.find("exports.A = A;").expect("class export");
        let variable = code.find("const x = 1;").expect("variable kept");
        let variable_export = code.find("exports.x = x;").expect("variable export");
        assert!(
            marker < class_at
                && class_at < class_export
                && class_export < variable
                && variable < variable_export,
            "{code}"
        );
    }

    #[test]
    fn commonjs_local_specifier_exports_assign_each_name() {
        // Same `exports.NAME = LOCAL;` identifier-assignment form as the
        // identifier default export in es6ImportDefaultBindingWithExport
        // server.js (adfd4388): `exports.default = a;`.
        let output = emit_with_options(
            "const a = 1, b = 2;\nexport { a, b as c };\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        let code = javascript(&output);
        assert!(code.contains("exports.a = a;"), "{code}");
        assert!(code.contains("exports.c = b;"), "{code}");
        assert!(!code.contains("export {"), "{code}");
    }

    #[test]
    fn commonjs_side_effect_imports_stay_as_require() {
        let output = emit_with_options(
            "import './polyfill';\nrun();\n",
            EmitOptions {
                target: ScriptTarget::Es2015,
                module: Some(crate::emitter::ModuleKind::CommonJs),
                ..EmitOptions::default()
            },
        );
        let code = javascript(&output);
        assert!(code.contains("require(\"./polyfill\");"), "{code}");
        assert!(!code.contains("import "), "{code}");
    }

    #[test]
    fn node_executes_lowered_computed_destructuring_assignment() {
        // computedPropertiesInDestructuring1 (50845e03) pins the preserved
        // ES6 forms; this snapshot carries no ES5 blob, so the ES5 lowering
        // contract is pinned behaviorally: values, and one evaluation per
        // key/default in source order.
        let source = r#"
const log = [];
let foo = "bar";
let bar = 0, bar3 = 0, bar4 = 0;
function key() { log.push("key"); return foo; }
function rhs() { log.push("rhs"); return { bar: "set" }; }
({ [key()]: bar } = rhs());
[{ [key()]: bar4 }] = [{ bar: "nested" }];
console.log(JSON.stringify([bar, bar4, log]));
"#;
        let output = emit_at(source, ScriptTarget::Es5);
        let code = javascript(&output);
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code()
                    == codes::DESTRUCTURING_ASSIGNMENT_REQUIRES_ES2015),
            "{:?}",
            output.diagnostics
        );
        // 3 = function definition `function key()` + 2 call sites (one per destructuring)
        assert_eq!(code.matches("key()").count(), 3, "{code}");
        // 2 = function definition `function rhs()` + 1 call site
        assert_eq!(code.matches("rhs()").count(), 2, "{code}");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bamts-cpd-{nonce}.cjs"));
        std::fs::write(&path, code).expect("write lowered JavaScript");
        let result = std::process::Command::new("node")
            .arg(&path)
            .output()
            .expect("execute Node");
        let _ = std::fs::remove_file(&path);
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout)
                .expect("Node stdout")
                .trim(),
            // Per spec, RHS evaluates before computed keys in destructuring assignment:
            // rhs() → key() → key()
            r#"["set","nested",["rhs","key","key"]]"#
        );
    }

    #[test]
    fn computed_destructuring_keys_evaluate_once_into_a_temp() {
        // Upstream computedPropertiesInDestructuring1 (50845e03) lowers
        // `let {[foo2()]: bar3} = ...` to a temp declaration plus a
        // bracket read, with the key evaluated exactly once.
        let output = emit_at(
            "const key = \"k\";\nconst { [key]: value } = { k: 1 };\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        let key_temp_at = code.find("let _t").expect("key temp declared");
        let digits: String = code[key_temp_at + "let _t".len()..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        let temp_name = format!("_t{digits}");
        let declaration_form = format!("let {temp_name} = key;");
        assert!(
            code.contains(&declaration_form),
            "key evaluates once into a temp ({declaration_form:?}): {code}"
        );
        assert!(
            code.contains(&format!("[{temp_name}]")),
            "binding reads through the temp: {code}"
        );
    }

    #[test]
    fn derived_constructor_keeps_prologue_before_super() {
        // Upstream constructorWithSuperAndPrologue.es5 (1e40213a): the
        // "ngInject" directive stays in the body ahead of the super call and
        // the super call becomes `return _super.call(this) || this;`.
        let output = emit_at(
            "class B extends A {\n    constructor() {\n        \"ngInject\";\n        console.log(\"B\");\n        super();\n    }\n}\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        let directive = code.find("ngInject").expect("directive survives");
        let super_call = code.find("super(").expect("super call survives");
        assert!(directive < super_call, "{code}");
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

    #[test]
    fn es5_downlevels_exponentiation_to_math_pow() {
        // Upstream exponentiationOperatorWithAnyAndNumber: every `**`
        // becomes `Math.pow(left, right)` below ES2016, and the nested
        // right-associative form nests the calls.
        let output = emit_at(
            "var a: any;\nvar b: number;\nvar r1 = a ** b;\nvar r2 = b ** 2;\nvar r3 = a ** b ** 2;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(code.contains("Math.pow(a, b)"), "{code}");
        assert!(code.contains("Math.pow(b, 2)"), "{code}");
        assert!(code.contains("Math.pow(a, Math.pow(b, 2))"), "{code}");
        assert!(!code.contains("**"), "{code}");
    }

    #[test]
    fn es5_downlevels_compound_exponentiation_identifiers() {
        // Upstream compoundExponentiationAssignmentLHSIsReference: identifier
        // targets become `x = Math.pow(x, value)` without temps.
        let output = emit_at(
            "var x = 2;\nvar value = 3;\nx **= value;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(code.contains("x = Math.pow(x, value)"), "{code}");
        assert!(!code.contains("**"), "{code}");
    }

    #[test]
    fn es5_compound_exponentiation_property_lhs_evaluates_base_once() {
        let output = emit_at(
            "var obj = { v: 2 };\nfunction pick() { return obj; }\npick().v **= 3;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(
            code.contains("(_t0 = pick()).v = Math.pow(_t0.v, 3)"),
            "{code}"
        );
        assert_eq!(code.matches("_t0 = pick()").count(), 1, "{code}");
        // Declaration plus the pick return; the lowering never re-evaluates it.
        assert_eq!(code.matches("obj").count(), 2, "{code}");
    }

    #[test]
    fn es5_compound_exponentiation_captures_computed_keys() {
        let output = emit_at(
            "var obj = { a: 2 };\nobj[\"a\"] **= 3;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(
            code.contains("(_t0 = obj)[_t1 = __propKey(\"a\")] = Math.pow(_t0[_t1], 3)"),
            "{code}"
        );
    }

    #[test]
    fn es5_declares_compound_exponentiation_temps_in_the_enclosing_function() {
        let output = emit_at(
            "var obj = { v: 2 };\nfunction f() { obj.v **= 3; }\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(code.contains("var _t0;"), "{code}");
        assert!(
            code.contains("(_t0 = obj).v = Math.pow(_t0.v, 3)"),
            "{code}"
        );
    }

    #[test]
    fn node_executes_lowered_compound_exponentiation_with_single_evaluations() {
        let output = emit_at(
            "var calls = 0;\nvar target = { v: 2 };\nfunction base() { calls += 1; return target; }\nfunction key() { calls += 10; return \"v\"; }\nbase()[key()] **= 3;\nconsole.log(calls + \":\" + target.v);\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(!code.contains("**"), "{code}");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bamts-exponentiation-{nonce}.cjs"));
        std::fs::write(&path, code).expect("write lowered JavaScript");
        let result = std::process::Command::new("node")
            .arg(&path)
            .output()
            .expect("execute Node");
        let _ = std::fs::remove_file(&path);
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        // The base runs once, the key runs once, and `2 ** 3` is `8`.
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("11:8"),
            "{code}"
        );
    }

    #[test]
    fn es2016_preserves_exponentiation_operators() {
        let source = "var r = a ** 2;\na **= 2;\n";
        let es2016_output = emit_at(source, ScriptTarget::Es2016);
        let es2016 = javascript(&es2016_output);
        assert!(es2016.contains("**"), "{es2016}");
        assert!(es2016.contains("**="), "{es2016}");
        assert!(!es2016.contains("Math.pow"), "{es2016}");
        let es2015_output = emit_at(source, ScriptTarget::Es2015);
        let es2015 = javascript(&es2015_output);
        assert!(es2015.contains("Math.pow(a, 2)"), "{es2015}");
        assert!(!es2015.contains("**"), "{es2015}");
    }

    #[test]
    fn es5_lowers_exponentiation_nested_in_expressions() {
        let output = emit_at(
            "var r = flag ? call(a ** 2) : obj[b ** 3];\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(code.contains("Math.pow(a, 2)"), "{code}");
        assert!(code.contains("Math.pow(b, 3)"), "{code}");
        assert!(!code.contains("**"), "{code}");
    }

    #[test]
    fn analysis_detects_exponentiation_nested_in_expressions() {
        let file = parse("var r = flag ? call(a ** 2) : b **= 3;\n");
        let options = options_for(ScriptTarget::Es5).transform_view();
        let plan = analyze(&file, &options);
        assert!(
            plan.features.contains(&LanguageFeature::Exponentiation),
            "{plan:?}"
        );
        assert!(
            plan.required.contains(&LanguageFeature::Exponentiation),
            "{plan:?}"
        );
        let plain = analyze(&parse("var r = a * b;\n"), &options);
        assert!(!plain.features.contains(&LanguageFeature::Exponentiation));
    }

    #[test]
    fn es5_rewrites_exponentiation_inside_synchronous_arrows() {
        let output = emit_at(
            "var expression = () => 2 ** 3;\n\
             var block = () => { var x = 2; x **= 3; return x; };\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert_eq!(code.matches("Math.pow").count(), 2, "{code}");
        assert!(!code.contains("**"), "{code}");
    }

    #[test]
    fn es5_rewrites_exponentiation_inside_parameter_initializers() {
        let output = emit_at(
            "function f(x = 2 ** 3) { return x; }\n\
             var g = (x = 3 ** 2) => x;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert_eq!(code.matches("Math.pow").count(), 2, "{code}");
        assert!(!code.contains("**"), "{code}");
    }

    #[test]
    fn es5_captures_computed_identifier_keys_before_getters() {
        let output = emit_at(
            "var key = \"a\";\n\
             var obj = { get a() { key = \"b\"; return 2; } };\n\
             obj[key] **= 3;\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        assert!(
            code.contains("(_t0 = obj)[_t1 = __propKey(key)] = Math.pow(_t0[_t1], 3)"),
            "{code}"
        );
    }

    #[test]
    fn es5_compound_exponentiation_coerces_computed_key_once() {
        let output = emit_at(
            "var coercions = 0;\n\
             var property = Symbol(\"x\");\n\
             var key = {};\n\
             key[Symbol.toPrimitive] = function (hint) {\n\
                 coercions += 1;\n\
                 if (hint !== \"string\") throw new Error(\"wrong hint\");\n\
                 return property;\n\
             };\n\
             var obj = {};\n\
             obj[property] = 2;\n\
             obj[key] **= 3;\n\
             console.log(coercions + \":\" + obj[property]);\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "1:8\n", "{code}");
    }

    #[test]
    fn es5_property_key_conversion_reads_ordinary_method_once() {
        let output = emit_at(
            "var reads = 0;\n\
             var calls = 0;\n\
             var key = {};\n\
             Object.defineProperty(key, \"toString\", {\n\
                 get: function () {\n\
                     reads += 1;\n\
                     return function () { calls += 1; return \"x\"; };\n\
                 }\n\
             });\n\
             var obj = { x: 2 };\n\
             obj[key] **= 2;\n\
             console.log(reads + \":\" + calls + \":\" + obj.x);\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "1:1:4\n", "{code}");
    }

    #[test]
    fn es5_property_key_conversion_without_symbol_uses_ordinary_methods() {
        let output = emit_at(
            "var Symbol = void 0;\n\
             var key = {};\n\
             var obj = { \"[object Object]\": 2 };\n\
             obj[key] **= 3;\n\
             console.log(obj[\"[object Object]\"]);\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "8\n", "{code}");
    }

    #[test]
    fn imported_helpers_keep_exact_property_key_conversion_inline() {
        let mut options = options_for(ScriptTarget::Es5);
        options.import_helpers = true;
        options.module = Some(ModuleKind::CommonJs);
        let output = emit_with_options(
            "var property = Symbol(\"x\");\n\
             var key = {};\n\
             key[Symbol.toPrimitive] = function () { return property; };\n\
             var obj = {};\n\
             obj[property] = 2;\n\
             obj[key] **= 3;\n\
             console.log(obj[property]);\n",
            options,
        );
        let code = javascript(&output);
        assert!(code.contains("function __propKey"), "{code}");
        assert!(!code.contains("require(\"tslib\").__propKey"), "{code}");
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "8\n", "{code}");
    }

    #[test]
    fn es2015_static_super_exponentiation_uses_math_pow() {
        let output = emit_at(
            "class Base { static x = 2; }\n\
             class Derived extends Base { static m() { super.x **= 3; } }\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("Math.pow"), "{code}");
        assert!(!code.contains("**"), "{code}");
    }
    #[test]
    fn es2015_static_block_super_exponentiation_uses_math_pow() {
        let output = emit_at(
            "class Base { static x = 2; }\n\
             class Derived extends Base { static { super.x **= 3; } }\n\
             console.log(Derived.x);\n",
            ScriptTarget::Es2015,
        );
        let code = javascript(&output);
        assert!(code.contains("Math.pow"), "{code}");
        assert!(!code.contains("**"), "{code}");
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "8\n", "{code}");
    }

    #[test]
    fn es5_parameter_compound_exponentiation_is_reentrant() {
        let output = emit_at(
            "var depth = 0;\n\
             var outer = { x: 2 };\n\
             var inner = { x: 5 };\n\
             function update(target, key, value = target[key] **= 2) { return value; }\n\
             var reentrant = {};\n\
             reentrant[Symbol.toPrimitive] = function () {\n\
                 if (depth++ === 0) update(inner, \"x\");\n\
                 return \"x\";\n\
             };\n\
             console.log(update(outer, reentrant) + \":\" + outer.x + \":\" + inner.x);\n",
            ScriptTarget::Es5,
        );
        let code = javascript(&output);
        let result = std::process::Command::new("node")
            .arg("-e")
            .arg(code)
            .output()
            .expect("execute Node");
        assert!(
            result.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            "4:4:25\n",
            "{code}"
        );
    }
}
