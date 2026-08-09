//! Compiler-owned frontend orchestration.
//!
//! [`compile_frontend`] drives the fixed scan -> parse -> check -> optional emit
//! pipeline over one source and returns a single recovered [`FrontendOutput`].
//! The pipeline never stops on user diagnostics: every stage always yields a
//! product, and their diagnostics are unioned into one canonically ordered,
//! duplicate-free vector without losing any distinct severity, range, or code.
//!
//! This module owns no filesystem, CLI, lowerer, runtime, or backend. It only
//! composes the existing scanner, parser, checker, and emitter surfaces.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::checker::{
    self, ProgramCheckInput, ProgramCheckOptions, ResolvedModuleEdge, SemanticModel,
};
use crate::diagnostic::{Diagnostic, Recovered};
use crate::emitter::{self, EmitOptions, EmitOutput};
use crate::lint::{LintProfile, LintTable};
use crate::parser;
use crate::program::{ModuleTarget, ResolvedProgram};
use crate::scanner;
use crate::source::{ScriptKind, SourceId, SourceText};
use crate::syntax::{ExportDeclaration, ExportNamedDeclaration, SourceFile};
use crate::telemetry::{Phase, Telemetry};

struct EdgeNode {
    id: crate::syntax::NodeId,
    range: crate::source::TextRange,
    children: Vec<Self>,
}

struct SourceEdgeNodeIndex {
    exact: std::collections::HashMap<crate::source::TextRange, crate::syntax::NodeId>,
    roots: Vec<EdgeNode>,
}

impl SourceEdgeNodeIndex {
    fn new(source: &SourceFile) -> Self {
        let mut exact = std::collections::HashMap::new();
        let roots = Self::statements(source.statements(), &mut exact);
        Self { exact, roots }
    }

    fn statements(
        statements: &[crate::syntax::Stmt],
        exact: &mut std::collections::HashMap<crate::source::TextRange, crate::syntax::NodeId>,
    ) -> Vec<EdgeNode> {
        statements
            .iter()
            .map(|statement| Self::statement(statement, exact))
            .collect()
    }

    fn statement(
        statement: &crate::syntax::Stmt,
        exact: &mut std::collections::HashMap<crate::source::TextRange, crate::syntax::NodeId>,
    ) -> EdgeNode {
        use crate::syntax::{ExportDefaultValue, ExternalModuleReference, FunctionBody, Statement};

        let id = statement.id();
        match statement.data() {
            Statement::Import(import) => {
                exact.insert(import.source.range(), id);
            }
            Statement::ImportEquals(import) => {
                if let ExternalModuleReference::Require(source) = &import.reference {
                    exact.insert(source.range(), id);
                }
            }
            Statement::Export(ExportDeclaration::All(export)) => {
                exact.insert(export.source.range(), id);
            }
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                source: Some(source),
                ..
            })) => {
                exact.insert(source.range(), id);
            }
            _ => {}
        }

        let mut children = match statement.data() {
            Statement::Function(function) => match &function.function.body {
                Some(FunctionBody::Block(block)) => {
                    Self::statements(&block.data().statements, exact)
                }
                _ => Vec::new(),
            },
            Statement::Namespace(namespace) => {
                Self::statements(&namespace.body.data().statements, exact)
            }
            Statement::Declare(inner)
            | Statement::Labeled(crate::syntax::LabeledStatement { body: inner, .. }) => {
                vec![Self::statement(inner, exact)]
            }
            Statement::Block(block) => Self::statements(&block.data().statements, exact),
            Statement::If(branch) => {
                let mut children = vec![Self::statement(&branch.consequent, exact)];
                if let Some(alternate) = &branch.alternate {
                    children.push(Self::statement(alternate, exact));
                }
                children
            }
            Statement::Switch(switch) => switch
                .cases
                .iter()
                .flat_map(|case| Self::statements(&case.data().consequent, exact))
                .collect(),
            Statement::For(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::ForIn(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::ForOf(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::While(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::DoWhile(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::Try(statement) => {
                let mut children = Self::statements(&statement.block.data().statements, exact);
                if let Some(handler) = &statement.handler {
                    children.extend(Self::statements(
                        &handler.data().body.data().statements,
                        exact,
                    ));
                }
                if let Some(finalizer) = &statement.finalizer {
                    children.extend(Self::statements(&finalizer.data().statements, exact));
                }
                children
            }
            Statement::With(statement) => vec![Self::statement(&statement.body, exact)],
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                inner,
            ))) => vec![Self::statement(inner, exact)],
            Statement::Export(ExportDeclaration::Default(default)) => match &default.value {
                ExportDefaultValue::Function(function) => match &function.body {
                    Some(FunctionBody::Block(block)) => {
                        Self::statements(&block.data().statements, exact)
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };

        // `smallest_containing` binary-searches this list by start position, so
        // source-order is a correctness requirement, not a convention. Sort
        // establishes the invariant by construction regardless of how each arm
        // assembled its children, keeping the search sound in release builds.
        children.sort_by_key(|node| node.range.start());

        EdgeNode {
            id,
            range: statement.range(),
            children,
        }
    }

    fn node_for(&self, range: crate::source::TextRange) -> Option<crate::syntax::NodeId> {
        self.exact
            .get(&range)
            .copied()
            .or_else(|| Self::smallest_containing(&self.roots, range))
    }

    fn smallest_containing(
        nodes: &[EdgeNode],
        range: crate::source::TextRange,
    ) -> Option<crate::syntax::NodeId> {
        let index = nodes.partition_point(|node| node.range.start() <= range.start());
        let node = nodes.get(index.checked_sub(1)?)?;
        (node.range.end() >= range.end())
            .then(|| Self::smallest_containing(&node.children, range).unwrap_or(node.id))
    }
}

/// The frontend product a caller wants produced for one source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrontendMode {
    /// Scan, parse, and type-check only; no emit is produced.
    Check,
    /// Additionally emit runtime JavaScript with type-only syntax erased.
    JavaScript,
    /// Additionally emit a TypeScript declaration file.
    Declaration,
}

impl FrontendMode {
    /// Returns the emit options this mode requests, or `None` for check-only.
    #[must_use]
    const fn emit_options(self) -> Option<EmitOptions> {
        match self {
            Self::Check => None,
            Self::JavaScript => Some(EmitOptions::javascript()),
            Self::Declaration => Some(EmitOptions::declaration()),
        }
    }
}

/// One immutable frontend compilation request.
#[derive(Clone, Debug)]
pub struct FrontendRequest {
    /// The compilation-assigned identity of the source.
    pub source_id: SourceId,
    /// The syntax accepted for the source.
    pub script_kind: ScriptKind,
    /// The shared, immutable source text.
    pub source: Arc<SourceText>,
    /// The frontend product to produce.
    pub mode: FrontendMode,
}

/// The immutable product of a full frontend compilation.
///
/// The recovered [`SourceFile`] and [`SemanticModel`] are always present, the
/// [`EmitOutput`] is present exactly when the request asked for one, and
/// [`FrontendOutput::diagnostics`] is the single canonically ordered union of
/// every stage's diagnostics.
pub struct FrontendOutput {
    mode: FrontendMode,
    source_file: SourceFile,
    semantic_model: SemanticModel,
    emit: Option<EmitOutput>,
    diagnostics: Vec<Diagnostic>,
}

impl FrontendOutput {
    /// Returns the mode this output was produced for.
    #[must_use]
    pub const fn mode(&self) -> FrontendMode {
        self.mode
    }

    /// Returns the recovered parser product, always present even on syntax errors.
    #[must_use]
    pub const fn source_file(&self) -> &SourceFile {
        &self.source_file
    }

    /// Returns the recovered semantic model, always present even on type errors.
    #[must_use]
    pub const fn semantic_model(&self) -> &SemanticModel {
        &self.semantic_model
    }

    /// Returns the emit product, present exactly when the request asked to emit.
    #[must_use]
    pub const fn emit(&self) -> Option<&EmitOutput> {
        self.emit.as_ref()
    }

    /// Returns every stage's diagnostics in canonical order with no duplicates.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Returns whether any diagnostic is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| !diagnostic.is_warning())
    }

    /// Consumes the output into its parts.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        SourceFile,
        SemanticModel,
        Option<EmitOutput>,
        Vec<Diagnostic>,
    ) {
        (
            self.source_file,
            self.semantic_model,
            self.emit,
            self.diagnostics,
        )
    }
}

/// Frontend products for every module of one canonical resolved program.
pub struct ProgramFrontendOutput {
    entrypoint: SourceId,
    modules: Vec<FrontendOutput>,
}

impl ProgramFrontendOutput {
    #[must_use]
    pub const fn entrypoint_id(&self) -> SourceId {
        self.entrypoint
    }

    /// Products in the same dependency-first order as the resolved program.
    #[must_use]
    pub fn modules(&self) -> &[FrontendOutput] {
        &self.modules
    }

    #[must_use]
    pub fn module(&self, source_id: SourceId) -> Option<&FrontendOutput> {
        self.modules
            .iter()
            .find(|output| output.source_file().source_id() == source_id)
    }
}

/// Runs the frontend for every module while preserving the graph's canonical identities.
#[must_use]
pub fn compile_program_frontend(
    program: &ResolvedProgram,
    mode: FrontendMode,
) -> ProgramFrontendOutput {
    compile_program_frontend_with_lints(program, mode, &LintTable::new(LintProfile::Default))
}

/// Runs the frontend for every module with caller-resolved lint levels.
#[must_use]
pub fn compile_program_frontend_with_lints(
    program: &ResolvedProgram,
    mode: FrontendMode,
    levels: &LintTable,
) -> ProgramFrontendOutput {
    Telemetry::measure(Phase::Total, || {
        let parsed = program
            .modules()
            .iter()
            .map(|module| {
                let source_id = module.source_id();
                let script_kind = module.script_kind();
                let source = Arc::clone(module.source());
                let scanned = Telemetry::measure(Phase::Scan, || {
                    scanner::scan(source_id, script_kind, source)
                });
                Telemetry::measure(Phase::Parse, || parser::parse(scanned))
            })
            .collect::<Vec<_>>();
        let edges = resolved_checker_edges(program, &parsed);
        let checked = Telemetry::measure(Phase::Check, || {
            let options = if program.is_commonjs() {
                ProgramCheckOptions::commonjs()
            } else {
                ProgramCheckOptions::standard()
            }
            .with_strict_null_checks(program.is_strict_null_checks())
            .with_no_implicit_any(program.is_no_implicit_any())
            .with_always_strict(program.is_always_strict())
            .with_target(if program.is_target_es5() {
                Some("es5")
            } else {
                None
            });
            checker::check_program_with_options(
                ProgramCheckInput {
                    files: &parsed,
                    edges: &edges,
                },
                levels,
                options,
            )
        });
        let (mut program_model, program_diagnostics) = checked.into_parts();
        // Partition program diagnostics once into source-keyed buckets that
        // preserve the canonical order from `Recovered`. The buckets are only
        // looked up by module source id below; the module iteration order (not
        // map iteration order) establishes output order, and diagnostics whose
        // source id matches no module are left in the map and dropped, matching
        // the previous per-module filter behavior.
        let mut buckets: BTreeMap<SourceId, Vec<Diagnostic>> = BTreeMap::new();
        for diagnostic in program_diagnostics {
            buckets
                .entry(diagnostic.source_id())
                .or_default()
                .push(diagnostic);
        }
        let modules = parsed
            .into_iter()
            .map(|parsed| {
                let source_id = parsed.product().source_id();
                let semantic_model = program_model
                    .remove_file(source_id)
                    .expect("whole-program checker returns every parsed module");
                let emit = mode.emit_options().map(|options| {
                    Telemetry::measure(Phase::Emit, || {
                        emitter::emit_checked(parsed.product(), &semantic_model, options)
                    })
                });
                let (source_file, mut diagnostics) = parsed.into_parts();
                if let Some(bucket) = buckets.remove(&source_id) {
                    diagnostics.extend(bucket);
                }
                if let Some(output) = &emit {
                    diagnostics.extend(output.diagnostics.iter().cloned());
                }
                FrontendOutput {
                    mode,
                    source_file,
                    semantic_model,
                    emit,
                    diagnostics: canonicalize(diagnostics),
                }
            })
            .collect();
        ProgramFrontendOutput {
            entrypoint: program.entrypoint_id(),
            modules,
        }
    })
}

fn resolved_checker_edges(
    program: &ResolvedProgram,
    files: &[Recovered<SourceFile>],
) -> Vec<ResolvedModuleEdge> {
    program
        .modules()
        .iter()
        .flat_map(|module| {
            let source = files
                .iter()
                .find(|file| file.product().source_id() == module.source_id())
                .expect("resolved module has one parsed source")
                .product();
            let nodes = SourceEdgeNodeIndex::new(source);
            module.dependencies().iter().filter_map(move |edge| {
                let ModuleTarget::Local(to) = edge.target() else {
                    return None;
                };
                Some(ResolvedModuleEdge {
                    from: module.source_id(),
                    specifier: nodes.node_for(edge.range())?,
                    to: *to,
                })
            })
        })
        .collect()
}

/// Runs the fixed frontend pipeline with settled default lint levels.
#[must_use]
pub fn compile_frontend(request: FrontendRequest) -> FrontendOutput {
    compile_frontend_with_lints(request, &LintTable::new(LintProfile::Default))
}

/// Runs the fixed scan -> parse -> check -> optional emit frontend pipeline
/// using the caller's resolved lint table.
///
/// Every stage runs regardless of the diagnostics its predecessor produced, so
/// the returned [`FrontendOutput`] always carries a recovered `SourceFile` and
/// `SemanticModel`, and (for emitting modes) an [`EmitOutput`] even when earlier
/// stages reported errors. All stage diagnostics are merged, canonically
/// ordered, and de-duplicated into one vector.
#[must_use]
pub fn compile_frontend_with_lints(request: FrontendRequest, levels: &LintTable) -> FrontendOutput {
    Telemetry::measure(Phase::Total, || {
        let FrontendRequest {
            source_id,
            script_kind,
            source,
            mode,
        } = request;

        let scanned = Telemetry::measure(Phase::Scan, || {
            scanner::scan(source_id, script_kind, source)
        });
        let parsed = Telemetry::measure(Phase::Parse, || parser::parse(scanned));
        let checked =
            Telemetry::measure(Phase::Check, || checker::check_with_lints(&parsed, levels));

        // Emit consumes the recovered tree and this pass's semantic model; it never
        // gates on prior diagnostics.
        let emit = mode.emit_options().map(|options| {
            Telemetry::measure(Phase::Emit, || {
                emitter::emit_checked(parsed.product(), checked.product(), options)
            })
        });

        let (source_file, parse_diagnostics) = parsed.into_parts();
        let (semantic_model, check_diagnostics) = checked.into_parts();

        let mut diagnostics = parse_diagnostics;
        diagnostics.extend(check_diagnostics);
        if let Some(output) = &emit {
            diagnostics.extend(output.diagnostics.iter().cloned());
        }
        let diagnostics = canonicalize(diagnostics);

        FrontendOutput {
            mode,
            source_file,
            semantic_model,
            emit,
            diagnostics,
        }
    })
}

/// Orders diagnostics by the canonical [`Diagnostic`] key and removes exact
/// duplicates.
///
/// The canonical order is a total order over `(source, range, code, severity,
/// message)`, so sorting groups every identical diagnostic and `dedup` collapses
/// only exact duplicates: two diagnostics differing in any of severity, range,
/// or code are never merged.
fn canonicalize(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort();
    diagnostics.dedup();
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::{FrontendMode, FrontendRequest, canonicalize, compile_frontend};
    use crate::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
    use crate::source::{ScriptKind, SourceId, SourceText, TextRange, Utf16Pos};
    use std::sync::Arc;

    fn request(source: &str, mode: FrontendMode) -> FrontendRequest {
        FrontendRequest {
            source_id: SourceId::new(0),
            script_kind: ScriptKind::TypeScript,
            source: Arc::new(
                SourceText::new(source).expect("test source fits the per-file budget"),
            ),
            mode,
        }
    }

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered range")
    }

    fn has_code(diagnostics: &[Diagnostic], code: &str) -> bool {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code().as_str() == code)
    }

    fn is_sorted_unique(diagnostics: &[Diagnostic]) -> bool {
        diagnostics.windows(2).all(|pair| pair[0] < pair[1])
    }

    #[test]
    fn merges_syntax_type_and_warning_diagnostics_into_one_ordered_vector() {
        // Warning (unchecked catch property access), a type error (string not
        // assignable to number), and a trailing syntax error (missing initializer)
        // all coexist in one source.
        let source =
            "try {} catch (e) { e.message }\nconst n: number = \"oops\";\nconst bad: number =";
        let output = compile_frontend(request(source, FrontendMode::Check));
        let diagnostics = output.diagnostics();

        // The three stages each contributed at least one diagnostic.
        assert!(has_code(diagnostics, "BAMTS-W005"), "warning stage missing");
        assert!(has_code(diagnostics, "BAMTS-C004"), "type stage missing");
        assert!(
            diagnostics.iter().any(
                |diagnostic| diagnostic.severity() == DiagnosticSeverity::Error
                    && diagnostic.code().as_str() != "BAMTS-C004"
            ),
            "syntax stage missing",
        );

        // Both severities survive the merge.
        assert!(diagnostics.iter().any(Diagnostic::is_warning));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| !diagnostic.is_warning())
        );

        // The single vector is canonically ordered and free of exact duplicates.
        assert!(is_sorted_unique(diagnostics), "diagnostics not canonical");
    }

    #[test]
    fn emits_despite_earlier_errors() {
        // A type error must not suppress the emit product.
        let source = "const n: number = \"oops\";";
        let output = compile_frontend(request(source, FrontendMode::JavaScript));

        assert!(output.has_errors(), "expected a type error");
        let emit = output.emit().expect("javascript mode must emit");
        assert!(
            emit.code.contains("oops"),
            "emit should still print the recovered program",
        );
    }

    #[test]
    fn check_mode_produces_no_emit_while_js_and_declaration_do() {
        let source = "let value: number = 1;";

        let check = compile_frontend(request(source, FrontendMode::Check));
        assert!(check.emit().is_none(), "check mode must not emit");

        let js = compile_frontend(request(source, FrontendMode::JavaScript));
        let js_emit = js.emit().expect("javascript mode emits");

        let declaration = compile_frontend(request(source, FrontendMode::Declaration));
        let declaration_emit = declaration.emit().expect("declaration mode emits");

        // JavaScript erases the type annotation; the declaration surface keeps it.
        assert!(!js_emit.code.contains("number"), "js must erase the type");
        assert!(
            declaration_emit.code.contains("number"),
            "declaration must retain the type",
        );
        assert_ne!(js_emit.code, declaration_emit.code);
    }

    #[test]
    fn canonicalize_collapses_exact_duplicates_and_preserves_distinct_ones() {
        let source_id = SourceId::new(0);
        let code = DiagnosticCode::new("BAMTS-C001");
        let base = Diagnostic::error(code, source_id, range(0, 1), "duplicate");
        let duplicate = base.clone();
        // Distinct in severity only.
        let as_warning = Diagnostic::warning(code, source_id, range(0, 1), "duplicate");
        // Distinct in range only.
        let elsewhere = Diagnostic::error(code, source_id, range(2, 3), "duplicate");
        // Distinct in code only.
        let other_code = Diagnostic::error(
            DiagnosticCode::new("BAMTS-C002"),
            source_id,
            range(0, 1),
            "duplicate",
        );

        let merged = canonicalize(vec![
            base.clone(),
            duplicate,
            as_warning.clone(),
            elsewhere.clone(),
            other_code.clone(),
            base.clone(),
        ]);

        // The two exact copies collapse to one; every distinct diagnostic remains.
        assert_eq!(merged.len(), 4);
        assert_eq!(merged.iter().filter(|d| **d == base).count(), 1);
        assert!(merged.contains(&as_warning));
        assert!(merged.contains(&elsewhere));
        assert!(merged.contains(&other_code));
        assert!(is_sorted_unique(&merged));
    }

    #[test]
    fn frontend_output_never_contains_duplicate_diagnostics() {
        let source =
            "try {} catch (e) { e.message }\nconst n: number = \"oops\";\nconst bad: number =";
        let output = compile_frontend(request(source, FrontendMode::JavaScript));
        assert!(is_sorted_unique(output.diagnostics()));
    }

    #[test]
    fn resolved_edges_keep_static_and_find_import_equals_and_nested_dynamic_imports() {
        use std::{
            fs,
            sync::atomic::{AtomicU64, Ordering},
        };

        use crate::{
            parser,
            program::ProgramLoader,
            project::{ProjectConfig, ProjectRoot},
            scanner,
            syntax::{Expression, FunctionBody, Statement},
        };

        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let root_path = std::env::temp_dir().join(format!(
            "bamts-pipeline-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root_path).unwrap();
        let write = |name: &str, source: &str| fs::write(root_path.join(name), source).unwrap();
        write(
            "main.ts",
            "import value from \"./static\"; import equal = require(\"./equal\"); async function nested() { return import(\"./dynamic\"); }",
        );
        write("static.ts", "export default 1;");
        write("equal.ts", "export = 1;");
        write("dynamic.ts", "export default 1;");

        let root = ProjectRoot::new(fs::canonicalize(&root_path).unwrap()).unwrap();
        let config = ProjectConfig::parse(&root, root_path.join("tsconfig.json"), "{}").unwrap();
        let program = ProgramLoader::new(&root, config.options())
            .unwrap()
            .load("main.ts")
            .unwrap();
        let files = program
            .modules()
            .iter()
            .map(|module| {
                parser::parse(scanner::scan(
                    module.source_id(),
                    module.script_kind(),
                    Arc::clone(module.source()),
                ))
            })
            .collect::<Vec<_>>();
        let edges = super::resolved_checker_edges(&program, &files);
        let source = files
            .iter()
            .find(|file| file.product().source_id() == program.entrypoint_id())
            .unwrap()
            .product();
        let statements = source.statements();
        let Statement::Function(function) = statements[2].data() else {
            panic!("expected nested function declaration");
        };
        let Some(FunctionBody::Block(body)) = &function.function.body else {
            panic!("expected function block");
        };
        let Statement::Return(return_statement) = body.data().statements[0].data() else {
            panic!("expected return statement");
        };
        let dynamic_import = return_statement.argument.as_ref().unwrap();
        assert!(matches!(dynamic_import.data(), Expression::Import(_)));

        assert_eq!(edges.len(), 3);
        assert!(
            edges
                .iter()
                .any(|edge| edge.specifier == statements[0].id())
        );
        assert!(
            edges
                .iter()
                .any(|edge| edge.specifier == statements[1].id())
        );
        assert!(
            edges
                .iter()
                .any(|edge| edge.specifier == body.data().statements[0].id())
        );

        fs::remove_dir_all(root_path).unwrap();
    }

    #[test]
    fn telemetry_records_frontend_phases_when_a_collector_is_active() {
        use crate::telemetry::{Phase, Telemetry, TelemetryCollector};

        // Disabled path: no collector, so nothing is timed and nothing panics.
        assert!(!Telemetry::enabled());
        let _ = compile_frontend(request("const n: number = 1;", FrontendMode::JavaScript));

        // Enabled path: a collector on this thread captures per-phase wall time.
        let collector = TelemetryCollector::start();
        let _ = compile_frontend(request(
            "const n: number = 1;\nfunction f(x: number): number { return x + 1; }",
            FrontendMode::JavaScript,
        ));
        let totals = collector.snapshot();
        drop(collector);

        assert!(
            totals.total > std::time::Duration::ZERO,
            "total wall recorded"
        );
        assert!(
            totals.get(Phase::Scan) > std::time::Duration::ZERO,
            "scan recorded"
        );
        assert!(
            totals.get(Phase::Parse) > std::time::Duration::ZERO,
            "parse recorded"
        );
        assert!(
            totals.get(Phase::Check) > std::time::Duration::ZERO,
            "check recorded"
        );
        // JavaScript mode emits, so the emit phase is timed too.
        assert!(
            totals.get(Phase::Emit) > std::time::Duration::ZERO,
            "emit recorded"
        );
        assert!(!Telemetry::enabled());
    }

    #[test]
    fn compile_program_frontend_attaches_one_model_and_ordered_unique_diagnostics_per_module() {
        use std::{
            fs,
            sync::atomic::{AtomicU64, Ordering},
        };

        use crate::{
            program::ProgramLoader,
            project::{ProjectConfig, ProjectRoot},
        };

        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let root_path = std::env::temp_dir().join(format!(
            "bamts-pipeline-multi-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root_path).unwrap();
        let write = |name: &str, source: &str| fs::write(root_path.join(name), source).unwrap();
        write(
            "main.ts",
            "import { value } from \"./dep\";\nconst n: number = \"oops\";",
        );
        write("dep.ts", "export const value: number = \"oops\";");

        let root = ProjectRoot::new(fs::canonicalize(&root_path).unwrap()).unwrap();
        let config = ProjectConfig::parse(&root, root_path.join("tsconfig.json"), "{}").unwrap();
        let program = ProgramLoader::new(&root, config.options())
            .unwrap()
            .load("main.ts")
            .unwrap();

        let expected_ids: Vec<SourceId> = program
            .modules()
            .iter()
            .map(|module| module.source_id())
            .collect();
        assert!(
            expected_ids.len() > 1,
            "fixture must load multiple modules"
        );

        let output = super::compile_program_frontend(&program, FrontendMode::Check);

        // Module order stays stable: outputs appear in the same dependency-first
        // order as the resolved program.
        let actual_ids: Vec<SourceId> = output
            .modules()
            .iter()
            .map(|module| module.source_file().source_id())
            .collect();
        assert_eq!(actual_ids, expected_ids, "module order must match program order");

        // One semantic model per module, and every diagnostic is attached to the
        // module that owns its source id.
        let mut seen: Vec<&Diagnostic> = Vec::new();
        for module in output.modules() {
            assert!(
                module.semantic_model().scopes().len() >= 1,
                "each module must carry its own semantic model"
            );
            assert!(
                module
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| diagnostic.code().as_str() == "BAMTS-C004"),
                "module {:?} missing its type-error diagnostic",
                module.source_file().source_id(),
            );
            for diagnostic in module.diagnostics() {
                assert_eq!(
                    diagnostic.source_id(),
                    module.source_file().source_id(),
                    "diagnostic leaked across module boundaries",
                );
            }
            assert!(
                is_sorted_unique(module.diagnostics()),
                "module diagnostics must be canonically ordered and unique",
            );
            seen.extend(module.diagnostics());
        }

        // No diagnostic is duplicated across modules.
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), before, "diagnostic duplicated across modules");

        fs::remove_dir_all(root_path).unwrap();
    }
}
