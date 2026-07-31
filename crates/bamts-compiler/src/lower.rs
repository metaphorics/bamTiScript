//! Direct lowering from the runtime TypeScript/JavaScript AST to verified
//! canonical bytecode ([`bamts_bytecode`]).
//!
//! # Scope
//!
//! This lowering targets the production 36-opcode instruction algebra (`u32`
//! register/constant/function/pc indices, register-keyed properties, variadic
//! calls through one arguments array, explicit closures with capture arrays,
//! the iterator protocol, environment access, module exports, and a
//! definite-initialization verifier). It expresses the dynamic runtime kernel
//! the corpus exercises without special-casing syntax: computed and private
//! property access, non-empty arrays and spread, iteration (`for`/`of`,
//! `for`/`in`, `for await`), destructuring, template literals, regular
//! expressions, globals, closures, classes with prototypes/accessors/private
//! names, `this`/`arguments`/`new.target`, generators via
//! [`Instruction::Suspend`], async via [`Instruction::Await`], and module
//! exports.
//!
//! No runtime construct that the instruction set can model is silently
//! approximated. The handful of forms that remain genuinely inexpressible in
//! this ISA (or that carry no runtime semantics at all) are reported as typed
//! [`UnsupportedConstruct`] values at their rejection sites; each is documented
//! there and does not occur in the executable corpus.
//!
//! ## Scope and environment model
//!
//! Module top-level bindings live in the module environment record, which this
//! lowering names through [`Instruction::LoadGlobal`]/[`Instruction::StoreGlobal`]
//! (and [`Instruction::TypeOfGlobal`] for `typeof` of a possibly-undeclared
//! name). Function-local bindings each own a fixed register *home*;
//! initialization and assignment copy into that home with [`Instruction::Move`]
//! so a binding read after a branch or across a loop back-edge is provably
//! initialized on every path, exactly what the verifier's
//! definite-initialization fixpoint requires. A nested function that reads a
//! function-local binding of an enclosing function captures it: the free
//! variables are computed syntactically, snapshotted into a captures array in
//! the enclosing function, and bound to the callee's leading capture registers.
//! Arrow functions additionally capture `this`, `arguments`, and `new.target`
//! from their lexical enclosing function.

#![allow(clippy::too_many_lines)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;

use bamts_bytecode::{
    AccessorKind, BigIntLiteral, BinaryOp, Constant, ConstantId, EcmaString, EcmaStringBuilder,
    ExceptionHandler, Function, FunctionFlags, FunctionId, Instruction, IteratorKind,
    MAX_BIGINT_BYTES, MAX_CONSTANTS, MAX_FUNCTIONS, MAX_INSTRUCTIONS, MAX_REGISTERS, Module,
    NumberBits, Pc, Register, UnaryOp, Verified, VerifyError,
};

pub use crate::program::{
    ExecutableModuleProvenance, ExecutableProgram, ProgramLowerError, ProgramLowerErrorKind,
    ProgramLowerPhase, lower_program,
};

use crate::source::{ScriptKind, SourceId, TextRange, Utf16Pos};
use crate::syntax::{
    ArrayBindingElement, ArrayElement, ArrowFunction, AssignmentArrayElement, AssignmentExpression,
    AssignmentMemberTarget, AssignmentObjectProperty, AssignmentOperator, AssignmentTarget,
    AssignmentTargetNode, AwaitExpression, BinaryExpression, BinaryOperator, BindingPattern, Block,
    BooleanLiteralNode, CallArgument, CallExpression, ClassDeclaration, ClassMember,
    ConditionalExpression, DoWhileStatement, ExportDeclaration, ExportDefaultValue,
    ExportNamedDeclaration, ExportSpecifierMode, Expr, Expression, ForBinding, ForInStatement,
    ForInitializer, ForOfMode, ForOfStatement, ForStatement, FunctionBody, FunctionLike,
    IdentifierNode, IfStatement, ImportBinding, ImportDeclaration, ImportSpecifierMode, Literal,
    LogicalExpression, LogicalOperator, MemberExpression, MemberProperty, MetaProperty,
    ModuleExportName, NewExpression, NodeKind, NumericLiteralNode, ObjectLiteral, ObjectMember,
    ParameterNode, Pattern, PrivateIdentifierNode, PropertyModifier, PropertyName,
    RegexLiteralNode, SourceFile, Statement, Stmt, StringLiteralNode, SwitchStatement,
    TemplateElementNode, TemplateLiteral, TokenKind, UnaryOperator, UpdateExpression,
    UpdateOperator, VariableDeclaration, VariableKind, WhileStatement, YieldExpression,
};

/// A degenerate range at the start of the document, used as the diagnostic
/// anchor for nodes whose own range is absent (missing syntax slots).
fn zero_range() -> TextRange {
    match TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO) {
        Ok(range) => range,
        Err(_) => unreachable!("Utf16Pos::ZERO is never after itself"),
    }
}

/// Caller-selected lowering mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LowerOptions {
    /// Accepts JavaScript [`ScriptKind`]s in addition to TypeScript ones.
    pub javascript_compatibility: bool,
}

/// The executable artifact this lowerer is producing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoweringGoal {
    /// A standalone module whose imports and exports are bytecode instructions.
    Module,
    /// A member of a linked program whose linkage lives in program metadata.
    ProgramModule,
    /// An ECMAScript classic script with no module syntax and a completion value.
    ClassicScript,
}

/// Structural production ceilings, mirroring the bytecode verifier's limits.
/// Two instruction slots are always reserved for a function's terminating
/// epilogue so a body can never leave no room for its own terminator.
const MAX_BODY_INSTRUCTIONS: usize = MAX_INSTRUCTIONS as usize - 2;
/// Persisted string constants must fit the deterministic decode ceiling so an
/// assembled module round-trips through [`bamts_bytecode::decode`].
const MAX_STRING_UNITS: usize = 1 << 20;

/// A typed lowering failure anchored to one source location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LowerError {
    pub source: SourceId,
    pub range: TextRange,
    pub kind: LowerErrorKind,
}

/// The closed set of lowering failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LowerErrorKind {
    /// A JavaScript source was lowered without `javascript_compatibility`.
    JavaScriptSourceNeedsCompatibility { script_kind: ScriptKind },
    /// JSON sources have no executable statement semantics.
    JsonSourceNotExecutable,
    /// A parser recovery node reached lowering.
    MissingSyntax { expected: NodeKind },
    /// A numeric literal lexeme did not denote a finite deterministic value.
    InvalidNumericLiteral,
    /// A bigint literal lexeme did not denote a canonical integer value.
    InvalidBigIntLiteral,
    /// A regular-expression literal lexeme was malformed.
    InvalidRegexLiteral,
    /// A module linkage name contained an unpaired UTF-16 surrogate.
    IllFormedMetadataString,
    /// A runtime construct the current instruction set cannot express.
    Unsupported(UnsupportedConstruct),
    /// A structural production capacity ran out.
    Capacity(CapacityLimit),
    /// The assembled module failed bytecode verification. Lowering maintains
    /// every verifier invariant by construction, so this is defensive.
    Verify(VerifyError),
}

/// Runtime syntax this instruction set cannot express faithfully, plus
/// source-goal early errors. Every variant names one rejected construct; there
/// is no catch-all. None of these occur in the executable corpus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedConstruct {
    /// `with` opens a dynamic scope the register model cannot represent.
    WithStatement,
    /// `using`/`await using` explicit resource management (no disposal opcode).
    UsingDeclaration,
    /// A labeled statement (no labeled control-flow target model).
    LabeledStatement,
    /// A labeled `break`/`continue`.
    LabeledJump,
    /// A runtime `enum` (const enums are type-only and already erased).
    EnumDeclaration,
    /// A runtime `namespace`/`module` block.
    NamespaceDeclaration,
    /// A runtime `import x = require(...)` / `import x = ns` declaration.
    RuntimeImportEquals,
    /// A runtime `export * from ...` (no dynamic per-name re-export).
    RuntimeExportAll,
    /// An `export =` assignment.
    ExportAssignment,
    /// A decorated declaration.
    DecoratedDeclaration,
    /// A dynamic `import(expr)` whose specifier is not a string literal.
    DynamicImportExpression,
    /// An import declaration is invalid in a classic script.
    ImportDeclarationInScript,
    /// An export declaration is invalid in a classic script.
    ExportDeclarationInScript,
    /// A dynamic import expression is invalid in a classic script.
    DynamicImportInScript,
    /// `import.meta` (no host meta-object primitive).
    ImportMeta,
    /// An identifier spelled with unicode escape sequences.
    EscapedIdentifier,
    /// A `return` at module top level.
    ReturnOutsideFunction,
    /// A derived constructor that is not an implicit constructor or a single
    /// direct top-level `super(...)` call.
    DerivedConstructorShape,
    /// A derived constructor references `this` before its direct `super(...)`.
    ThisBeforeDerivedSuper,
}

/// The exhausted structural capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLimit {
    Registers,
    Constants,
    Functions,
    Instructions,
    StringUnits,
    BigIntBytes,
    BigIntWork,
    Captures,
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lowering failed in source {} at {}..{}: {}",
            self.source.get(),
            self.range.start().get(),
            self.range.end().get(),
            self.kind
        )
    }
}

impl fmt::Display for LowerErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JavaScriptSourceNeedsCompatibility { script_kind } => write!(
                f,
                "{script_kind:?} source requires LowerOptions::javascript_compatibility"
            ),
            Self::JsonSourceNotExecutable => f.write_str("JSON sources are not executable"),
            Self::MissingSyntax { expected } => {
                write!(f, "parser recovery produced a missing {expected:?}")
            }
            Self::InvalidNumericLiteral => f.write_str("numeric literal has no cooked value"),
            Self::InvalidBigIntLiteral => f.write_str("bigint literal has no canonical value"),
            Self::InvalidRegexLiteral => f.write_str("regular-expression literal is malformed"),
            Self::IllFormedMetadataString => {
                f.write_str("module metadata string is not well-formed UTF-16")
            }
            Self::Unsupported(construct) => {
                write!(f, "unsupported runtime semantics: {construct}")
            }
            Self::Capacity(limit) => write!(f, "bytecode capacity exhausted: {limit}"),
            Self::Verify(error) => write!(f, "assembled module failed verification: {error}"),
        }
    }
}

impl fmt::Display for UnsupportedConstruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::WithStatement => "`with` statement",
            Self::UsingDeclaration => "`using` declaration",
            Self::LabeledStatement => "labeled statement",
            Self::LabeledJump => "labeled `break`/`continue`",
            Self::EnumDeclaration => "runtime `enum` declaration",
            Self::NamespaceDeclaration => "runtime `namespace` declaration",
            Self::RuntimeImportEquals => "runtime `import =` declaration",
            Self::RuntimeExportAll => "runtime `export *` declaration",
            Self::ExportAssignment => "`export =` assignment",
            Self::DecoratedDeclaration => "decorated declaration",
            Self::DynamicImportExpression => "dynamic `import()` with a non-literal specifier",
            Self::ImportDeclarationInScript => "`import` declaration in a classic script",
            Self::ExportDeclarationInScript => "`export` declaration in a classic script",
            Self::DynamicImportInScript => "dynamic `import()` in a classic script",
            Self::ImportMeta => "`import.meta` meta property",
            Self::EscapedIdentifier => "identifier containing escape sequences",
            Self::ReturnOutsideFunction => "top-level `return`",
            Self::DerivedConstructorShape => {
                "derived constructor without one direct `super(...)` call"
            }
            Self::ThisBeforeDerivedSuper => "`this` before `super(...)` in a derived constructor",
        };
        f.write_str(text)
    }
}

impl fmt::Display for CapacityLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Registers => "too many registers in one function",
            Self::Constants => "too many pooled constants",
            Self::Functions => "too many functions",
            Self::Instructions => "too many instructions in one function",
            Self::StringUnits => "string constant exceeds the deterministic pool code-unit ceiling",
            Self::BigIntBytes => "bigint constant exceeds the canonical decoder byte ceiling",
            Self::BigIntWork => "bigint radix conversion exceeds its deterministic work ceiling",
            Self::Captures => "too many captured variables in one closure",
        };
        f.write_str(text)
    }
}

impl Error for LowerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            LowerErrorKind::Verify(error) => Some(error),
            _ => None,
        }
    }
}

/// Lowers a parsed source file directly to a verified bytecode module.
///
/// Top-level statements become the entry function; every nested function
/// becomes one additional module function. The returned module has passed
/// [`Module::verify`].
///
/// # Errors
/// Returns a typed [`LowerError`] for an unsupported source kind, a parser
/// recovery node, an inexpressible runtime construct, an exhausted capacity, or
/// (defensively) a verification failure.
pub fn lower(file: &SourceFile, options: LowerOptions) -> Result<Module<Verified>, LowerError> {
    let module = assemble(file, options)?;
    module.verify().map_err(|error| LowerError {
        source: file.source_id(),
        range: file.range(),
        kind: LowerErrorKind::Verify(error),
    })
}

/// Assembles the module without the final verification pass. Exposed for tests
/// that need to inspect assembled-but-unverified bytecode.
pub(crate) fn assemble(
    file: &SourceFile,
    options: LowerOptions,
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    assemble_with_linkage_strings(file, options, &[], LoweringGoal::Module)
}

pub(crate) fn assemble_program_module(
    file: &SourceFile,
    options: LowerOptions,
    linkage_strings: &[String],
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    assemble_with_linkage_strings(file, options, linkage_strings, LoweringGoal::ProgramModule)
}

/// Assembles a classic script without the final verification pass.
pub(crate) fn assemble_classic_script_named(
    file: &SourceFile,
    options: LowerOptions,
    module_name: &str,
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    assemble_with_linkage_strings(
        file,
        options,
        &[module_name.to_owned()],
        LoweringGoal::ClassicScript,
    )
}

fn assemble_with_linkage_strings(
    file: &SourceFile,
    options: LowerOptions,
    linkage_strings: &[String],
    goal: LoweringGoal,
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    validate_script_kind(file, options)?;

    let mut builder = ModuleBuilder {
        source: file.source_id(),
        constants: Vec::new(),
        functions: Vec::new(),
    };
    for value in linkage_strings {
        builder.intern(Constant::String(EcmaString::from_utf8(value)), file.range())?;
    }
    let entry = builder.reserve_function(file.range())?;

    let mut context = FunctionContext::new_top_level(file, goal);
    let completion = if goal == LoweringGoal::ClassicScript {
        let completion = context.alloc_register(file.range())?;
        let undefined = context.undefined(&mut builder, file.range())?;
        context.move_to(file.range(), completion, undefined)?;
        context.completion = Some(completion);
        Some(completion)
    } else {
        None
    };
    context.lower_top_level(&mut builder, file.statements())?;
    match completion {
        Some(value) => context.emit(file.range(), Instruction::Return { value })?,
        None => context.emit(file.range(), Instruction::Halt)?,
    };
    let assembled = context.into_function(None, FunctionFlags::default());
    builder.fill_function(entry, assembled);

    let functions = builder
        .functions
        .into_iter()
        .map(|slot| slot.expect("every reserved function slot is filled before assembly"))
        .collect();
    Ok(Module::new(builder.constants, functions, entry))
}

fn validate_script_kind(file: &SourceFile, options: LowerOptions) -> Result<(), LowerError> {
    let kind = match file.script_kind() {
        ScriptKind::TypeScript | ScriptKind::TypeScriptReact => return Ok(()),
        ScriptKind::JavaScript | ScriptKind::JavaScriptReact => {
            if options.javascript_compatibility {
                return Ok(());
            }
            LowerErrorKind::JavaScriptSourceNeedsCompatibility {
                script_kind: file.script_kind(),
            }
        }
        ScriptKind::Json => LowerErrorKind::JsonSourceNotExecutable,
    };
    Err(LowerError {
        source: file.source_id(),
        range: file.range(),
        kind,
    })
}

/// Module-wide constant pool and function table.
struct ModuleBuilder {
    source: SourceId,
    constants: Vec<Constant>,
    functions: Vec<Option<Function>>,
}

impl ModuleBuilder {
    fn error(&self, range: TextRange, kind: LowerErrorKind) -> LowerError {
        LowerError {
            source: self.source,
            range,
            kind,
        }
    }

    /// Interns one constant, deduplicated, in deterministic first-use order.
    fn intern(&mut self, constant: Constant, range: TextRange) -> Result<ConstantId, LowerError> {
        if let Constant::String(value) = &constant
            && value.len_units() > MAX_STRING_UNITS
        {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::StringUnits)));
        }
        if let Some(position) = self
            .constants
            .iter()
            .position(|existing| *existing == constant)
        {
            return Ok(ConstantId::new(position as u32));
        }
        if self.constants.len() >= MAX_CONSTANTS as usize {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Constants)));
        }
        let id = ConstantId::new(self.constants.len() as u32);
        self.constants.push(constant);
        Ok(id)
    }

    fn reserve_function(&mut self, range: TextRange) -> Result<FunctionId, LowerError> {
        if self.functions.len() >= MAX_FUNCTIONS as usize {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Functions)));
        }
        let id = FunctionId::new(self.functions.len() as u32);
        self.functions.push(None);
        Ok(id)
    }

    fn fill_function(&mut self, id: FunctionId, function: Function) {
        self.functions[id.get() as usize] = Some(function);
    }
}

/// What a resolved name denotes inside the current function. Its storage kind is
/// fixed at declaration and never changes.
#[derive(Clone, Copy)]
enum Binding {
    /// A value living in a fixed register (the binding's home).
    Local(Register),
    /// A one-element array shared by every closure over this binding.
    Cell(Register),
}

type BindingSite = (usize, usize);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum BindingIdentity {
    Function(String),
    Lexical(BindingSite),
}

#[derive(Default)]
struct CapturePlan {
    captured: HashSet<BindingIdentity>,
    runtime_cells: HashSet<BindingIdentity>,
}

impl CapturePlan {
    fn captures(&self, name: &str, site: BindingSite, declaration_scope: DeclarationScope) -> bool {
        self.captured
            .contains(&binding_identity(name, site, declaration_scope))
    }

    fn requires_cell(
        &self,
        name: &str,
        site: BindingSite,
        declaration_scope: DeclarationScope,
    ) -> bool {
        let identity = binding_identity(name, site, declaration_scope);
        self.captured.contains(&identity) || self.runtime_cells.contains(&identity)
    }
}

#[derive(Clone)]
struct ImmediateDeclaration<'a> {
    name: String,
    site: BindingSite,
    range: TextRange,
    kind: ImmediateDeclarationKind<'a>,
}

#[derive(Clone, Copy)]
enum ImmediateDeclarationKind<'a> {
    Lexical,
    Function(&'a FunctionLike),
}

fn binding_identity(
    name: &str,
    site: BindingSite,
    declaration_scope: DeclarationScope,
) -> BindingIdentity {
    match declaration_scope {
        DeclarationScope::Function => BindingIdentity::Function(name.to_owned()),
        DeclarationScope::Lexical | DeclarationScope::Iteration => BindingIdentity::Lexical(site),
    }
}

fn binding_site(range: TextRange) -> BindingSite {
    (range.start().get(), range.end().get())
}

#[derive(Clone, Copy)]
enum DeclarationScope {
    Function,
    Lexical,
    Iteration,
}

/// One captured value of a nested function, in deterministic capture order.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CaptureKey {
    /// A named function-local binding of the enclosing function.
    Name(String),
    /// The enclosing function's `this` (arrow capture).
    This,
    /// The enclosing function's `arguments` (arrow capture).
    Arguments,
    /// The enclosing function's `new.target` (arrow capture).
    NewTarget,
    /// The parent constructor captured by a derived class constructor.
    Parent(Register),
}

/// How the current function observes its `arguments` binding.
#[derive(Clone, Copy)]
enum ArgumentsSource {
    /// A regular function: `arguments` is the activation's own exotic object.
    Own,
    /// An arrow (or nested arrow): `arguments` is a captured register.
    Captured(Register),
    /// Module top level or an arrow with no enclosing function: `arguments` is
    /// an ordinary free name resolved against the environment.
    None,
}

/// A live loop's break/continue placeholder jumps, patched when the loop ends.
struct LoopFrame {
    breaks: Vec<Pc>,
    continues: Vec<Pc>,
    is_loop: bool,
}

/// Completion kinds routed through a `finally` block.
const COMPLETION_NORMAL: i32 = 0;
const COMPLETION_RETURN: i32 = 1;
const COMPLETION_THROW: i32 = 2;
const COMPLETION_BREAK: i32 = 3;
const COMPLETION_CONTINUE: i32 = 4;

/// A live `finally` block: the completion state registers and the pending
/// jumps into the finally entry that must be patched once its PC is known.
struct FinallyFrame {
    kind_reg: Register,
    value_reg: Register,
    pending: Vec<Pc>,
    /// Loop-stack depth when this finally was entered; a `break`/`continue`
    /// whose target loop predates the finally must route through it.
    loop_depth: usize,
}

/// Per-function lowering state: code, register allocator, and lexical scopes.
struct FunctionContext<'a> {
    file: &'a SourceFile,
    code: Vec<Instruction>,
    registers: u32,
    capture_count: u32,
    parameter_count: u32,
    scopes: Vec<HashMap<String, Binding>>,
    /// Cells allocated before a declaration-owned initializer or body is built,
    /// indexed by the scanner's exact binding identity.
    predeclared_cells: HashMap<BindingIdentity, Register>,
    capture_plan: CapturePlan,
    loops: Vec<LoopFrame>,
    handlers: Vec<ExceptionHandler>,
    finally_stack: Vec<FinallyFrame>,
    /// `true` for the module entry function, whose bindings are the module
    /// environment (named globals) rather than register homes.
    top_level: bool,
    goal: LoweringGoal,
    /// Innermost statement-value target, present only for a classic script entry.
    completion: Option<Register>,
    /// Reusable statement-value registers indexed by normalizing-statement depth.
    completion_pool: Vec<Register>,
    completion_depth: usize,
    /// `Some(reg)` when `this` is captured by value by an arrow; `None` when
    /// the activation owns `this` (`LoadThis`).
    this_capture: Option<Register>,
    /// `Some(reg)` when `new.target` is captured by value by an arrow; `None`
    /// when owned (`LoadNewTarget`).
    new_target_capture: Option<Register>,
    /// The parent constructor captured by a derived constructor.
    parent_constructor_capture: Option<Register>,
    arguments_source: ArgumentsSource,
}

impl<'a> FunctionContext<'a> {
    fn new_top_level(file: &'a SourceFile, goal: LoweringGoal) -> Self {
        let capture_plan = CapturePlan::for_statements(file, file.statements());
        Self {
            file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            predeclared_cells: HashMap::new(),
            capture_plan,
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: true,
            goal,
            completion: None,
            completion_pool: Vec::new(),
            completion_depth: 0,
            this_capture: None,
            new_target_capture: None,
            parent_constructor_capture: None,
            arguments_source: ArgumentsSource::None,
        }
    }

    fn into_function(self, name: Option<ConstantId>, flags: FunctionFlags) -> Function {
        Function::new(
            name,
            self.capture_count,
            self.parameter_count,
            self.registers,
            flags,
            self.code,
            self.handlers,
        )
    }

    fn error(&self, range: TextRange, kind: LowerErrorKind) -> LowerError {
        LowerError {
            source: self.file.source_id(),
            range,
            kind,
        }
    }

    fn unsupported(&self, range: TextRange, construct: UnsupportedConstruct) -> LowerError {
        self.error(range, LowerErrorKind::Unsupported(construct))
    }

    fn missing(&self, range: TextRange, expected: NodeKind) -> LowerError {
        self.error(range, LowerErrorKind::MissingSyntax { expected })
    }

    // ------------------------------------------------------------------
    // Emission primitives
    // ------------------------------------------------------------------

    fn emit(&mut self, range: TextRange, instruction: Instruction) -> Result<Pc, LowerError> {
        if self.code.len() >= MAX_BODY_INSTRUCTIONS {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Instructions)));
        }
        let pc = Pc::new(self.code.len() as u32);
        self.code.push(instruction);
        Ok(pc)
    }

    fn next_pc(&self) -> Pc {
        Pc::new(self.code.len() as u32)
    }

    fn patch_jump(&mut self, at: Pc, target: Pc) {
        match &mut self.code[at.get() as usize] {
            Instruction::Jump { target: slot }
            | Instruction::JumpIfTrue { target: slot, .. }
            | Instruction::JumpIfFalse { target: slot, .. } => *slot = target,
            other => unreachable!("patch target of non-jump instruction: {other:?}"),
        }
    }

    fn alloc_register(&mut self, range: TextRange) -> Result<Register, LowerError> {
        if self.registers >= MAX_REGISTERS {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Registers)));
        }
        let register = Register::new(self.registers);
        self.registers += 1;
        Ok(register)
    }

    fn load_constant(
        &mut self,
        builder: &mut ModuleBuilder,
        constant: Constant,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        let id = builder.intern(constant, range)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::LoadConst { dst, constant: id })?;
        Ok(dst)
    }

    fn undefined(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        self.load_constant(builder, Constant::Undefined, range)
    }

    /// Loads a string constant into a fresh register (used for property keys,
    /// global names, and string values).
    fn string_reg(
        &mut self,
        builder: &mut ModuleBuilder,
        value: EcmaString,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        self.load_constant(builder, Constant::String(value), range)
    }

    fn move_to(
        &mut self,
        range: TextRange,
        dst: Register,
        src: Register,
    ) -> Result<(), LowerError> {
        self.emit(range, Instruction::Move { dst, src })?;
        Ok(())
    }

    fn lower_normalizing_statement(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        lower: impl FnOnce(&mut Self, &mut ModuleBuilder) -> Result<(), LowerError>,
    ) -> Result<(), LowerError> {
        let Some(outer) = self.completion else {
            return lower(self, builder);
        };
        let depth = self.completion_depth;
        let inner = match self.completion_pool.get(depth).copied() {
            Some(register) => register,
            None => {
                let register = self.alloc_register(range)?;
                self.completion_pool.push(register);
                register
            }
        };
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, inner, undefined)?;
        self.completion = Some(inner);
        self.completion_depth += 1;
        let result = lower(self, builder);
        self.completion_depth -= 1;
        self.completion = Some(outer);
        result?;
        self.move_to(range, outer, inner)
    }

    fn lower_without_completion(
        &mut self,
        builder: &mut ModuleBuilder,
        lower: impl FnOnce(&mut Self, &mut ModuleBuilder) -> Result<(), LowerError>,
    ) -> Result<(), LowerError> {
        let completion = self.completion.take();
        let result = lower(self, builder);
        self.completion = completion;
        result
    }

    // ------------------------------------------------------------------
    // Scopes and names
    // ------------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn resolve(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn declare(&mut self, name: String, binding: Binding, declaration_scope: DeclarationScope) {
        let scope = match declaration_scope {
            DeclarationScope::Function => self
                .scopes
                .first_mut()
                .expect("a function context always holds its root scope"),
            DeclarationScope::Lexical | DeclarationScope::Iteration => self
                .scopes
                .last_mut()
                .expect("a function context always holds at least one scope"),
        };
        scope.insert(name, binding);
    }

    fn identifier_text(&self, identifier: &IdentifierNode) -> Result<String, LowerError> {
        let token = identifier.data().token();
        if token.is_missing() {
            return Err(self.missing(identifier.range(), NodeKind::Identifier));
        }
        let Some(text) = self.file.token_text(token) else {
            return Err(self.missing(identifier.range(), NodeKind::Identifier));
        };
        if text.contains('\\') {
            return Err(
                self.unsupported(identifier.range(), UnsupportedConstruct::EscapedIdentifier)
            );
        }
        Ok(text.to_owned())
    }

    fn private_text(&self, private: &PrivateIdentifierNode) -> Result<String, LowerError> {
        let token = private.data().token();
        if token.is_missing() {
            return Err(self.missing(private.range(), NodeKind::PrivateIdentifier));
        }
        let Some(text) = self.file.token_text(token) else {
            return Err(self.missing(private.range(), NodeKind::PrivateIdentifier));
        };
        Ok(text.to_owned())
    }

    /// Declares an initialized function-local binding or module global.
    fn declare_initialized(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        value: Register,
        range: TextRange,
        site: BindingSite,
        declaration_scope: DeclarationScope,
    ) -> Result<(), LowerError> {
        if self.top_level && !matches!(declaration_scope, DeclarationScope::Iteration) {
            let id = builder.intern(Constant::String(EcmaString::from_utf8(name)), range)?;
            self.emit(range, Instruction::StoreGlobal { name: id, value })?;
            return Ok(());
        }
        if self.capture_plan.captures(name, site, declaration_scope) {
            let cell = self.alloc_register(range)?;
            self.emit(range, Instruction::CreateArray { dst: cell })?;
            self.emit(range, Instruction::ArrayPush { array: cell, value })?;
            self.declare(name.to_owned(), Binding::Cell(cell), declaration_scope);
        } else {
            let home = self.alloc_register(range)?;
            self.move_to(range, home, value)?;
            self.declare(name.to_owned(), Binding::Local(home), declaration_scope);
        }
        Ok(())
    }

    /// Seeds the cell a declaration-owned closure needs before its initializer
    /// or body can materialize that closure. Module globals stay environment-backed.
    fn predeclare_captured_binding(
        &mut self,
        name: &str,
        range: TextRange,
        site: BindingSite,
        declaration_scope: DeclarationScope,
    ) -> Result<(), LowerError> {
        if self.top_level
            || !self
                .capture_plan
                .requires_cell(name, site, declaration_scope)
        {
            return Ok(());
        }
        let identity = binding_identity(name, site, declaration_scope);
        if self.predeclared_cells.contains_key(&identity) {
            return Ok(());
        }
        if matches!(declaration_scope, DeclarationScope::Function)
            && let Some(Binding::Cell(cell)) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(name).copied())
        {
            self.predeclared_cells.insert(identity, cell);
            return Ok(());
        }
        let cell = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateCell { dst: cell })?;
        self.declare(name.to_owned(), Binding::Cell(cell), declaration_scope);
        self.predeclared_cells.insert(identity, cell);
        Ok(())
    }

    /// Predeclares just the planned captured leaves of a lexical pattern.
    fn predeclare_captured_pattern(
        &mut self,
        pattern: &Pattern,
        declaration_scope: DeclarationScope,
    ) -> Result<(), LowerError> {
        match pattern.data() {
            BindingPattern::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.predeclare_captured_binding(
                    &name,
                    identifier.range(),
                    binding_site(identifier.range()),
                    declaration_scope,
                )
            }
            BindingPattern::Object(object) => {
                for property in &object.properties {
                    self.predeclare_captured_pattern(&property.binding, declaration_scope)?;
                }
                Ok(())
            }
            BindingPattern::Array(array) => {
                for element in &array.elements {
                    if let ArrayBindingElement::Binding(pattern) = element {
                        self.predeclare_captured_pattern(pattern, declaration_scope)?;
                    }
                }
                Ok(())
            }
            BindingPattern::Assignment(assignment) => {
                self.predeclare_captured_pattern(&assignment.left, declaration_scope)
            }
            BindingPattern::Rest(rest) => {
                self.predeclare_captured_pattern(&rest.argument, declaration_scope)
            }
            BindingPattern::Missing(_) => Ok(()),
        }
    }

    /// Creates the class expression's own TDZ cell, keyed by its declaration
    /// site so a caller that already predeclared it shares the same binding.
    fn predeclare_class_expression_binding(
        &mut self,
        name: &str,
        range: TextRange,
        site: BindingSite,
    ) -> Result<Register, LowerError> {
        let identity = binding_identity(name, site, DeclarationScope::Lexical);
        if let Some(cell) = self.predeclared_cells.get(&identity).copied() {
            return Ok(cell);
        }
        let cell = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateCell { dst: cell })?;
        self.declare(
            name.to_owned(),
            Binding::Cell(cell),
            DeclarationScope::Lexical,
        );
        self.predeclared_cells.insert(identity, cell);
        Ok(cell)
    }

    fn cell_value(
        &mut self,
        builder: &mut ModuleBuilder,
        cell: Register,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        let key = self.load_constant(builder, Constant::Int32(0), range)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetProperty {
                dst,
                object: cell,
                key,
            },
        )?;
        Ok(dst)
    }

    fn store_cell(
        &mut self,
        builder: &mut ModuleBuilder,
        cell: Register,
        value: Register,
        range: TextRange,
    ) -> Result<(), LowerError> {
        let key = self.load_constant(builder, Constant::Int32(0), range)?;
        self.emit(
            range,
            Instruction::SetProperty {
                object: cell,
                key,
                value,
            },
        )?;
        Ok(())
    }

    fn rebind_iteration_cell(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        range: TextRange,
    ) -> Result<(), LowerError> {
        let Some(Binding::Cell(cell)) = self.resolve(name) else {
            return Ok(());
        };
        let value = self.cell_value(builder, cell, range)?;
        self.emit(range, Instruction::CreateArray { dst: cell })?;
        self.emit(range, Instruction::ArrayPush { array: cell, value })?;
        Ok(())
    }

    fn declaration_names(&self, declaration: &VariableDeclaration) -> Vec<String> {
        let mut names = Vec::new();
        for declarator in &declaration.declarations {
            collect_pattern_names(self.file, &declarator.data().binding, &mut names);
        }
        names
    }

    fn rebind_iteration_cells(
        &mut self,
        builder: &mut ModuleBuilder,
        names: &[String],
        range: TextRange,
    ) -> Result<(), LowerError> {
        for name in names {
            self.rebind_iteration_cell(builder, name, range)?;
        }
        Ok(())
    }

    /// Stores a produced value into a binding named `name`, reusing a hoisted
    /// function binding or declaring a fresh binding with immutable storage.
    fn store_binding(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        value: Register,
        range: TextRange,
        site: BindingSite,
        declaration_scope: DeclarationScope,
    ) -> Result<(), LowerError> {
        let identity = binding_identity(name, site, declaration_scope);
        if let Some(cell) = self.predeclared_cells.get(&identity).copied() {
            return self.store_cell(builder, cell, value, range);
        }
        if matches!(declaration_scope, DeclarationScope::Function)
            && let Some(binding) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(name).copied())
        {
            return match binding {
                Binding::Local(home) => self.move_to(range, home, value),
                Binding::Cell(cell) => self.store_cell(builder, cell, value, range),
            };
        }
        self.declare_initialized(builder, name, value, range, site, declaration_scope)
    }

    /// Pre-allocates and zero-initializes storage for every `var`-scoped binding.
    fn hoist_vars(
        &mut self,
        builder: &mut ModuleBuilder,
        statements: &[Stmt],
        range: TextRange,
    ) -> Result<(), LowerError> {
        if self.top_level {
            return Ok(());
        }
        let mut names = Vec::new();
        collect_var_names(self.file, statements, &mut names);
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            if self
                .scopes
                .first()
                .is_some_and(|scope| scope.contains_key(&name))
            {
                continue;
            }
            let id = builder.intern(Constant::Undefined, range)?;
            if self
                .capture_plan
                .captures(&name, binding_site(range), DeclarationScope::Function)
            {
                let value = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::LoadConst {
                        dst: value,
                        constant: id,
                    },
                )?;
                let cell = self.alloc_register(range)?;
                self.emit(range, Instruction::CreateArray { dst: cell })?;
                self.emit(range, Instruction::ArrayPush { array: cell, value })?;
                self.declare(name, Binding::Cell(cell), DeclarationScope::Function);
            } else {
                let home = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::LoadConst {
                        dst: home,
                        constant: id,
                    },
                )?;
                self.declare(name, Binding::Local(home), DeclarationScope::Function);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Reads and assignments of names
    // ------------------------------------------------------------------

    fn read_name(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        if let Some(binding) = self.resolve(name) {
            return match binding {
                Binding::Local(register) => Ok(register),
                Binding::Cell(cell) => self.cell_value(builder, cell, range),
            };
        }
        if name == "arguments"
            && let Some(register) = self.arguments_value(builder, range)?
        {
            return Ok(register);
        }
        if name == "undefined" {
            return self.undefined(builder, range);
        }
        // Free name: read from the environment.
        let id = builder.intern(Constant::String(EcmaString::from_utf8(name)), range)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::LoadGlobal { dst, name: id })?;
        Ok(dst)
    }

    /// Stores `value` into the binding named `name` (already existing), or the
    /// environment if the name is free.
    fn assign_name(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        value: Register,
        range: TextRange,
    ) -> Result<(), LowerError> {
        if let Some(binding) = self.resolve(name) {
            return match binding {
                Binding::Local(home) => self.move_to(range, home, value),
                Binding::Cell(cell) => {
                    let _ = self.cell_value(builder, cell, range)?;
                    self.store_cell(builder, cell, value, range)
                }
            };
        }
        let id = builder.intern(Constant::String(EcmaString::from_utf8(name)), range)?;
        self.emit(range, Instruction::StoreGlobal { name: id, value })?;
        Ok(())
    }

    /// The current value of a name for read-modify-write (compound assignment
    /// and update). Returns the register holding the current value.
    fn read_name_value(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        self.read_name(builder, name, range)
    }

    // ------------------------------------------------------------------
    // this / arguments / new.target
    // ------------------------------------------------------------------

    fn this_value(&mut self, range: TextRange) -> Result<Register, LowerError> {
        if let Some(register) = self.this_capture {
            return Ok(register);
        }
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::LoadThis { dst })?;
        Ok(dst)
    }

    fn new_target_value(&mut self, range: TextRange) -> Result<Register, LowerError> {
        if let Some(register) = self.new_target_capture {
            return Ok(register);
        }
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::LoadNewTarget { dst })?;
        Ok(dst)
    }

    /// Returns the `arguments` register if the current function provides one,
    /// or `None` if `arguments` should be treated as a free name.
    fn arguments_value(
        &mut self,
        _builder: &mut ModuleBuilder,
        range: TextRange,
    ) -> Result<Option<Register>, LowerError> {
        match self.arguments_source {
            ArgumentsSource::Own => {
                let dst = self.alloc_register(range)?;
                self.emit(range, Instruction::LoadArguments { dst })?;
                Ok(Some(dst))
            }
            ArgumentsSource::Captured(register) => Ok(Some(register)),
            ArgumentsSource::None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Top-level lowering
    // ------------------------------------------------------------------

    fn lower_top_level(
        &mut self,
        builder: &mut ModuleBuilder,
        statements: &[Stmt],
    ) -> Result<(), LowerError> {
        self.instantiate_declarations(builder, statements, false)?;
        for statement in statements {
            self.lower_statement(builder, statement)?;
        }
        Ok(())
    }

    fn instantiate_declarations(
        &mut self,
        builder: &mut ModuleBuilder,
        statements: &[Stmt],
        switch_scope: bool,
    ) -> Result<(), LowerError> {
        let declarations = collect_immediate_declarations(self.file, statements);
        for declaration in &declarations {
            if matches!(&declaration.kind, ImmediateDeclarationKind::Lexical)
                && (switch_scope
                    || self.capture_plan.requires_cell(
                        &declaration.name,
                        declaration.site,
                        DeclarationScope::Lexical,
                    ))
            {
                self.predeclare_captured_binding(
                    &declaration.name,
                    declaration.range,
                    declaration.site,
                    DeclarationScope::Lexical,
                )?;
            }
        }
        for declaration in declarations {
            if let ImmediateDeclarationKind::Function(function) = declaration.kind {
                self.instantiate_function_declaration(
                    builder,
                    declaration.range,
                    &declaration.name,
                    declaration.site,
                    function,
                )?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Statements
    // ------------------------------------------------------------------

    fn lower_statement(
        &mut self,
        builder: &mut ModuleBuilder,
        statement: &Stmt,
    ) -> Result<(), LowerError> {
        let range = statement.range();
        match statement.data() {
            Statement::Interface(_) | Statement::TypeAlias(_) | Statement::Declare(_) => Ok(()),
            Statement::Import(import) => self.lower_import(builder, range, import),
            Statement::ImportEquals(import) => {
                if self.goal == LoweringGoal::ClassicScript {
                    Err(self.unsupported(range, UnsupportedConstruct::ImportDeclarationInScript))
                } else if import.is_type_only {
                    Ok(())
                } else {
                    Err(self.unsupported(range, UnsupportedConstruct::RuntimeImportEquals))
                }
            }
            Statement::Export(export) => self.lower_export(builder, range, export),
            Statement::Variable(declaration) => {
                self.lower_variable_declaration(builder, declaration)
            }
            Statement::Function(_) => Ok(()),
            Statement::Class(class) => self.lower_class_declaration(builder, range, class, None),
            Statement::Enum(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::EnumDeclaration))
            }
            Statement::Namespace(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::NamespaceDeclaration))
            }
            Statement::Block(block) => {
                self.push_scope();
                let result = self.lower_block(builder, block.data());
                self.pop_scope();
                result
            }
            Statement::Empty => Ok(()),
            Statement::Expression(expression) => {
                let value = self.lower_expression(builder, &expression.expression)?;
                if let Some(completion) = self.completion {
                    self.move_to(range, completion, value)?;
                }
                Ok(())
            }
            Statement::If(if_statement) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_if(builder, if_statement)
                })
            }
            Statement::Switch(switch) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_switch(builder, range, switch)
                })
            }
            Statement::For(for_statement) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_for(builder, for_statement)
                })
            }
            Statement::ForIn(for_in) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_for_in(builder, range, for_in)
                })
            }
            Statement::ForOf(for_of) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_for_of(builder, range, for_of)
                })
            }
            Statement::While(while_statement) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_while(builder, while_statement)
                })
            }
            Statement::DoWhile(do_while) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_do_while(builder, do_while)
                })
            }
            Statement::Try(try_statement) => {
                self.lower_normalizing_statement(builder, range, |this, builder| {
                    this.lower_try(builder, range, try_statement)
                })
            }
            Statement::With(_) => Err(self.unsupported(range, UnsupportedConstruct::WithStatement)),
            Statement::Labeled(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::LabeledStatement))
            }
            Statement::Break(jump) => self.lower_break(builder, range, jump.label.is_some()),
            Statement::Continue(jump) => self.lower_continue(builder, range, jump.label.is_some()),
            Statement::Return(return_statement) => {
                if self.top_level {
                    return Err(
                        self.unsupported(range, UnsupportedConstruct::ReturnOutsideFunction)
                    );
                }
                let value = match &return_statement.argument {
                    Some(expression) => self.lower_expression(builder, expression)?,
                    None => self.undefined(builder, range)?,
                };
                if self.route_through_finally(builder, range, COMPLETION_RETURN, Some(value))? {
                    return Ok(());
                }
                self.emit(range, Instruction::Return { value })?;
                Ok(())
            }
            Statement::Throw(throw) => {
                let value = self.lower_expression(builder, &throw.argument)?;
                self.emit(range, Instruction::Throw { value })?;
                Ok(())
            }
            Statement::Debugger => Ok(()),
            Statement::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }

    fn lower_block(
        &mut self,
        builder: &mut ModuleBuilder,
        block: &Block,
    ) -> Result<(), LowerError> {
        self.instantiate_declarations(builder, &block.statements, false)?;
        for statement in &block.statements {
            self.lower_statement(builder, statement)?;
        }
        Ok(())
    }

    fn lower_nested(&mut self, builder: &mut ModuleBuilder, body: &Stmt) -> Result<(), LowerError> {
        self.push_scope();
        let result = self.lower_statement(builder, body);
        self.pop_scope();
        result
    }

    fn lower_if(
        &mut self,
        builder: &mut ModuleBuilder,
        if_statement: &IfStatement,
    ) -> Result<(), LowerError> {
        let range = if_statement.test.range();
        let condition = self.lower_expression(builder, &if_statement.test)?;
        let to_else = self.emit(
            range,
            Instruction::JumpIfFalse {
                condition,
                target: Pc::new(0),
            },
        )?;
        self.lower_nested(builder, &if_statement.consequent)?;
        match &if_statement.alternate {
            Some(alternate) => {
                let to_end = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
                let else_pc = self.next_pc();
                self.patch_jump(to_else, else_pc);
                self.lower_nested(builder, alternate)?;
                let end_pc = self.next_pc();
                self.patch_jump(to_end, end_pc);
            }
            None => {
                let end_pc = self.next_pc();
                self.patch_jump(to_else, end_pc);
            }
        }
        Ok(())
    }

    fn lower_while(
        &mut self,
        builder: &mut ModuleBuilder,
        while_statement: &WhileStatement,
    ) -> Result<(), LowerError> {
        let range = while_statement.test.range();
        let head = self.next_pc();
        let condition = self.lower_expression(builder, &while_statement.test)?;
        let exit_jump = self.emit(
            range,
            Instruction::JumpIfFalse {
                condition,
                target: Pc::new(0),
            },
        )?;
        self.loops.push(LoopFrame {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_loop: true,
        });
        self.lower_nested(builder, &while_statement.body)?;
        self.emit(range, Instruction::Jump { target: head })?;
        let exit = self.next_pc();
        self.patch_jump(exit_jump, exit);
        let frame = self.loops.pop().expect("loop frame is balanced");
        for jump in frame.breaks {
            self.patch_jump(jump, exit);
        }
        for jump in frame.continues {
            self.patch_jump(jump, head);
        }
        Ok(())
    }

    fn lower_do_while(
        &mut self,
        builder: &mut ModuleBuilder,
        do_while: &DoWhileStatement,
    ) -> Result<(), LowerError> {
        let range = do_while.test.range();
        let head = self.next_pc();
        self.loops.push(LoopFrame {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_loop: true,
        });
        self.lower_nested(builder, &do_while.body)?;
        let test_pc = self.next_pc();
        let condition = self.lower_expression(builder, &do_while.test)?;
        self.emit(
            range,
            Instruction::JumpIfTrue {
                condition,
                target: head,
            },
        )?;
        let exit = self.next_pc();
        let frame = self.loops.pop().expect("loop frame is balanced");
        for jump in frame.breaks {
            self.patch_jump(jump, exit);
        }
        for jump in frame.continues {
            self.patch_jump(jump, test_pc);
        }
        Ok(())
    }

    fn lower_for(
        &mut self,
        builder: &mut ModuleBuilder,
        for_statement: &ForStatement,
    ) -> Result<(), LowerError> {
        self.push_scope();
        let result = self.lower_for_inner(builder, for_statement);
        self.pop_scope();
        result
    }

    fn lower_for_inner(
        &mut self,
        builder: &mut ModuleBuilder,
        for_statement: &ForStatement,
    ) -> Result<(), LowerError> {
        let per_iteration_names = match &for_statement.initializer {
            Some(ForInitializer::Variable(declaration))
                if matches!(declaration.kind, VariableKind::Let | VariableKind::Const) =>
            {
                self.declaration_names(declaration)
            }
            _ => Vec::new(),
        };
        if let Some(initializer) = &for_statement.initializer {
            match initializer {
                ForInitializer::Variable(declaration) => {
                    if matches!(declaration.kind, VariableKind::Let | VariableKind::Const) {
                        self.lower_iteration_variable_declaration(builder, declaration)?;
                    } else {
                        self.lower_variable_declaration(builder, declaration)?;
                    }
                }
                ForInitializer::Expression(expression) => {
                    self.lower_expression(builder, expression)?;
                }
            }
        }
        let head = self.next_pc();
        let exit_jump = match &for_statement.test {
            Some(test) => {
                let condition = self.lower_expression(builder, test)?;
                Some(self.emit(
                    test.range(),
                    Instruction::JumpIfFalse {
                        condition,
                        target: Pc::new(0),
                    },
                )?)
            }
            None => None,
        };
        self.loops.push(LoopFrame {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_loop: true,
        });
        self.lower_nested(builder, &for_statement.body)?;
        let update_pc = self.next_pc();
        self.rebind_iteration_cells(builder, &per_iteration_names, head_range(for_statement))?;
        if let Some(update) = &for_statement.update {
            self.lower_expression(builder, update)?;
        }
        self.emit(
            head_range(for_statement),
            Instruction::Jump { target: head },
        )?;
        let exit = self.next_pc();
        if let Some(jump) = exit_jump {
            self.patch_jump(jump, exit);
        }
        let frame = self.loops.pop().expect("loop frame is balanced");
        for jump in frame.breaks {
            self.patch_jump(jump, exit);
        }
        for jump in frame.continues {
            self.patch_jump(jump, update_pc);
        }
        Ok(())
    }

    /// Lowers `for (binding in object)` via the enumerate-keys iterator.
    fn lower_for_in(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        for_in: &ForInStatement,
    ) -> Result<(), LowerError> {
        let subject = self.lower_expression(builder, &for_in.object)?;
        self.lower_iteration(
            builder,
            range,
            subject,
            IteratorKind::Keys,
            &for_in.binding,
            &for_in.body,
        )
    }

    /// Lowers `for (binding of iterable)` and `for await (binding of iterable)`.
    fn lower_for_of(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        for_of: &ForOfStatement,
    ) -> Result<(), LowerError> {
        let subject = self.lower_expression(builder, &for_of.iterable)?;
        let kind = match for_of.mode {
            ForOfMode::Sync => IteratorKind::Sync,
            ForOfMode::Async => IteratorKind::Async,
        };
        self.lower_iteration(builder, range, subject, kind, &for_of.binding, &for_of.body)
    }

    /// Shared iterator-driven loop for `for`/`of`, `for`/`in`, and
    /// `for await`/`of`. Async iteration splits each step into
    /// [`Instruction::IteratorStep`] → [`Instruction::Await`] →
    /// [`Instruction::IteratorResult`]; sync loops keep the fused
    /// [`Instruction::IteratorNext`].
    fn lower_iteration(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        subject: Register,
        kind: IteratorKind,
        binding: &ForBinding,
        body: &Stmt,
    ) -> Result<(), LowerError> {
        self.push_scope();
        match binding {
            ForBinding::Variable(declaration)
                if matches!(
                    declaration.kind,
                    VariableKind::Using | VariableKind::AwaitUsing
                ) =>
            {
                self.pop_scope();
                return Err(self.unsupported(range, UnsupportedConstruct::UsingDeclaration));
            }
            _ => {}
        }
        let iterator = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetIterator {
                dst: iterator,
                src: subject,
                kind,
            },
        )?;
        let done = self.alloc_register(range)?;
        let value = self.alloc_register(range)?;
        let head = self.next_pc();
        if kind == IteratorKind::Async {
            // `for await`: step the async iterator to its raw (thenable)
            // result, suspend until it settles, then read `done`/`value` from
            // the settled iterator result object. The awaited register is
            // defined by `Await` on the resume edge, which is exactly the
            // fall-through pc `IteratorResult` executes at.
            let step = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::IteratorStep {
                    dst: step,
                    iterator,
                },
            )?;
            let settled = self.emit_await(range, step)?;
            self.emit(
                range,
                Instruction::IteratorResult {
                    done,
                    value,
                    result: settled,
                },
            )?;
        } else {
            self.emit(
                range,
                Instruction::IteratorNext {
                    done,
                    value,
                    iterator,
                },
            )?;
        }
        let exit_jump = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: done,
                target: Pc::new(0),
            },
        )?;
        self.loops.push(LoopFrame {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_loop: true,
        });
        // Fresh per-iteration scope for the loop binding.
        self.push_scope();
        self.bind_for_binding(builder, binding, value, range)?;
        let body_result = self.lower_statement(builder, body);
        self.pop_scope();
        body_result?;
        self.emit(range, Instruction::Jump { target: head })?;
        let exit = self.next_pc();
        self.patch_jump(exit_jump, exit);
        let frame = self.loops.pop().expect("loop frame is balanced");
        for jump in frame.breaks {
            self.patch_jump(jump, exit);
        }
        for jump in frame.continues {
            self.patch_jump(jump, head);
        }
        self.pop_scope();
        Ok(())
    }

    fn bind_for_binding(
        &mut self,
        builder: &mut ModuleBuilder,
        binding: &ForBinding,
        value: Register,
        range: TextRange,
    ) -> Result<(), LowerError> {
        match binding {
            ForBinding::Variable(declaration) => {
                let declaration_scope = match declaration.kind {
                    VariableKind::Var => DeclarationScope::Function,
                    VariableKind::Let | VariableKind::Const => DeclarationScope::Iteration,
                    VariableKind::Using | VariableKind::AwaitUsing => {
                        return Err(self.unsupported(range, UnsupportedConstruct::UsingDeclaration));
                    }
                };
                let declarator = declaration
                    .declarations
                    .first()
                    .ok_or_else(|| self.missing(range, NodeKind::VariableDeclarator))?;
                self.bind_pattern(
                    builder,
                    &declarator.data().binding,
                    value,
                    declaration_scope,
                )
            }
            ForBinding::Target(target) => self.assign_target(builder, target, value),
        }
    }

    fn lower_break(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        labeled: bool,
    ) -> Result<(), LowerError> {
        if labeled {
            return Err(self.unsupported(range, UnsupportedConstruct::LabeledJump));
        }
        if self.route_through_finally(builder, range, COMPLETION_BREAK, None)? {
            return Ok(());
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let frame = self.loops.last_mut().ok_or_else(|| LowerError {
            source: self.file.source_id(),
            range,
            kind: LowerErrorKind::MissingSyntax {
                expected: NodeKind::BreakStatement,
            },
        })?;
        frame.breaks.push(jump);
        Ok(())
    }

    fn lower_continue(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        labeled: bool,
    ) -> Result<(), LowerError> {
        if labeled {
            return Err(self.unsupported(range, UnsupportedConstruct::LabeledJump));
        }
        if self.route_through_finally(builder, range, COMPLETION_CONTINUE, None)? {
            return Ok(());
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let index = self.nearest_loop_index().ok_or_else(|| LowerError {
            source: self.file.source_id(),
            range,
            kind: LowerErrorKind::MissingSyntax {
                expected: NodeKind::ContinueStatement,
            },
        })?;
        self.loops[index].continues.push(jump);
        Ok(())
    }

    /// Routes an abrupt completion (`return`/`break`/`continue`) through the
    /// innermost enclosing `finally`, if one is live and the completion crosses
    /// it. Returns `true` when the completion was routed.
    fn route_through_finally(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        kind: i32,
        value: Option<Register>,
    ) -> Result<bool, LowerError> {
        let Some((kind_reg, value_reg, depth)) = self
            .finally_stack
            .last()
            .map(|frame| (frame.kind_reg, frame.value_reg, frame.loop_depth))
        else {
            return Ok(false);
        };
        // Determine the frame this completion targets; if that frame predates
        // the finally, the completion crosses it and routes through it.
        let target = match kind {
            COMPLETION_BREAK => {
                if self.loops.is_empty() {
                    return Ok(false);
                }
                Some(self.loops.len() - 1)
            }
            COMPLETION_CONTINUE => match self.nearest_loop_index() {
                Some(index) => Some(index),
                None => return Ok(false),
            },
            _ => None,
        };
        if let Some(target) = target
            && target >= depth
        {
            return Ok(false);
        }
        let marker = self.load_constant(builder, Constant::Int32(kind), range)?;
        self.move_to(range, kind_reg, marker)?;
        if let Some(value) = value {
            self.move_to(range, value_reg, value)?;
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        self.finally_stack
            .last_mut()
            .expect("finally frame present")
            .pending
            .push(jump);
        Ok(true)
    }

    /// The index of the innermost enclosing real loop (skipping `switch`
    /// break-scopes), which is where a `continue` transfers.
    fn nearest_loop_index(&self) -> Option<usize> {
        self.loops.iter().rposition(|frame| frame.is_loop)
    }

    fn lower_switch(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        switch: &SwitchStatement,
    ) -> Result<(), LowerError> {
        let discriminant = self.lower_expression(builder, &switch.discriminant)?;
        self.push_scope();
        let switch_statements = switch
            .cases
            .iter()
            .flat_map(|case| case.data().consequent.iter().cloned())
            .collect::<Vec<_>>();
        self.instantiate_declarations(builder, &switch_statements, true)?;
        self.loops.push(LoopFrame {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_loop: false,
        });
        let mut case_jumps: Vec<Option<Pc>> = Vec::with_capacity(switch.cases.len());
        let mut default_index = None;
        for (index, case) in switch.cases.iter().enumerate() {
            match &case.data().test {
                Some(test) => {
                    let value = self.lower_expression(builder, test)?;
                    let matched = self.alloc_register(range)?;
                    self.emit(
                        range,
                        Instruction::Binary {
                            dst: matched,
                            op: BinaryOp::StrictEqual,
                            left: discriminant,
                            right: value,
                        },
                    )?;
                    let jump = self.emit(
                        range,
                        Instruction::JumpIfTrue {
                            condition: matched,
                            target: Pc::new(0),
                        },
                    )?;
                    case_jumps.push(Some(jump));
                }
                None => {
                    default_index = Some(index);
                    case_jumps.push(None);
                }
            }
        }
        let no_match_jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let mut body_starts: Vec<Pc> = Vec::with_capacity(switch.cases.len());
        for case in &switch.cases {
            let start = self.next_pc();
            body_starts.push(start);
            for statement in &case.data().consequent {
                self.lower_statement(builder, statement)?;
            }
        }
        let exit = self.next_pc();
        for (jump, start) in case_jumps.iter().zip(body_starts.iter()) {
            if let Some(jump) = jump {
                self.patch_jump(*jump, *start);
            }
        }
        match default_index {
            Some(index) => self.patch_jump(no_match_jump, body_starts[index]),
            None => self.patch_jump(no_match_jump, exit),
        }
        let frame = self.loops.pop().expect("switch break frame is balanced");

        for jump in frame.breaks {
            self.patch_jump(jump, exit);
        }
        self.pop_scope();
        Ok(())
    }

    fn lower_try(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        try_statement: &crate::syntax::TryStatement,
    ) -> Result<(), LowerError> {
        let has_finally = try_statement
            .finalizer
            .as_ref()
            .is_some_and(|finalizer| !finalizer.data().statements.is_empty());
        if has_finally {
            return self.lower_try_finally(builder, range, try_statement);
        }
        let Some(handler_clause) = &try_statement.handler else {
            self.push_scope();
            let result = self.lower_block(builder, try_statement.block.data());
            self.pop_scope();
            return result;
        };

        let start = self.next_pc();
        self.push_scope();
        let block_result = self.lower_block(builder, try_statement.block.data());
        self.pop_scope();
        block_result?;
        let end = self.next_pc();
        if end.get() == start.get() {
            return Ok(());
        }
        let over_catch = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;

        let catch_register = self.alloc_register(range)?;
        let handler_pc = self.next_pc();
        if let Some(completion) = self.completion {
            let undefined = self.undefined(builder, range)?;
            self.move_to(range, completion, undefined)?;
        }
        self.push_scope();
        let clause = handler_clause.data();
        if let Some(binding) = &clause.binding {
            let bind_result =
                self.bind_pattern(builder, binding, catch_register, DeclarationScope::Lexical);
            if let Err(error) = bind_result {
                self.pop_scope();
                return Err(error);
            }
        }
        let catch_result = self.lower_block(builder, clause.body.data());
        self.pop_scope();
        catch_result?;
        let after = self.next_pc();
        self.patch_jump(over_catch, after);

        self.handlers.push(ExceptionHandler {
            start,
            end,
            handler: handler_pc,
            catch_register,
        });
        Ok(())
    }

    /// Lowers `try`/`finally` (with an optional `catch`) by routing every
    /// completion of the protected body through the finally block, then
    /// dispatching the recorded completion after it runs.
    fn lower_try_finally(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        try_statement: &crate::syntax::TryStatement,
    ) -> Result<(), LowerError> {
        let finalizer = try_statement
            .finalizer
            .as_ref()
            .expect("lower_try_finally is only called with a non-empty finalizer");
        let kind_reg = self.alloc_register(range)?;
        let value_reg = self.alloc_register(range)?;
        let normal = self.load_constant(builder, Constant::Int32(COMPLETION_NORMAL), range)?;
        self.move_to(range, kind_reg, normal)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, value_reg, undefined)?;
        self.finally_stack.push(FinallyFrame {
            kind_reg,
            value_reg,
            pending: Vec::new(),
            loop_depth: self.loops.len(),
        });

        let start = self.next_pc();
        self.push_scope();
        let body_result = self.lower_block(builder, try_statement.block.data());
        self.pop_scope();
        if let Err(error) = body_result {
            self.finally_stack.pop();
            return Err(error);
        }
        let end = self.next_pc();
        // Normal completion of the try body routes to the finally.
        self.push_finally_completion(builder, range, COMPLETION_NORMAL, None)?;

        let handler = if end.get() == start.get() {
            None
        } else {
            let catch_register = self.alloc_register(range)?;
            let handler_pc = self.next_pc();
            if let Some(handler_clause) = &try_statement.handler {
                if let Some(completion) = self.completion {
                    let undefined = self.undefined(builder, range)?;
                    self.move_to(range, completion, undefined)?;
                }
                self.push_scope();
                let clause = handler_clause.data();
                if let Some(binding) = &clause.binding {
                    let bind_result = self.bind_pattern(
                        builder,
                        binding,
                        catch_register,
                        DeclarationScope::Lexical,
                    );
                    if let Err(error) = bind_result {
                        self.pop_scope();
                        self.finally_stack.pop();
                        return Err(error);
                    }
                }
                let catch_result = self.lower_block(builder, clause.body.data());
                self.pop_scope();
                if let Err(error) = catch_result {
                    self.finally_stack.pop();
                    return Err(error);
                }
                self.push_finally_completion(builder, range, COMPLETION_NORMAL, None)?;
            } else {
                // No catch: record the thrown value and re-raise after finally.
                self.push_finally_completion(
                    builder,
                    range,
                    COMPLETION_THROW,
                    Some(catch_register),
                )?;
            }
            Some((catch_register, handler_pc))
        };

        let frame = self.finally_stack.pop().expect("finally frame present");
        let finally_pc = self.next_pc();
        for jump in &frame.pending {
            self.patch_jump(*jump, finally_pc);
        }
        if let Some((catch_register, handler_pc)) = handler {
            self.handlers.push(ExceptionHandler {
                start,
                end,
                handler: handler_pc,
                catch_register,
            });
        }
        self.push_scope();
        let finally_result = self.lower_without_completion(builder, |this, builder| {
            this.lower_block(builder, finalizer.data())
        });
        self.pop_scope();
        finally_result?;
        self.emit_finally_dispatch(builder, range, kind_reg, value_reg)
    }

    /// Records a completion (sets the state registers) and jumps to the pending
    /// finally entry.
    fn push_finally_completion(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        kind: i32,
        value: Option<Register>,
    ) -> Result<(), LowerError> {
        let (kind_reg, value_reg) = {
            let frame = self.finally_stack.last().expect("finally frame present");
            (frame.kind_reg, frame.value_reg)
        };
        let marker = self.load_constant(builder, Constant::Int32(kind), range)?;
        self.move_to(range, kind_reg, marker)?;
        if let Some(value) = value {
            self.move_to(range, value_reg, value)?;
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        self.finally_stack
            .last_mut()
            .expect("finally frame present")
            .pending
            .push(jump);
        Ok(())
    }

    /// After a finally block runs, resumes the recorded completion.
    fn emit_finally_dispatch(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        kind_reg: Register,
        value_reg: Register,
    ) -> Result<(), LowerError> {
        // return
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_RETURN)?;
        if !self.route_through_finally(builder, range, COMPLETION_RETURN, Some(value_reg))? {
            self.emit(range, Instruction::Return { value: value_reg })?;
        }
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // throw
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_THROW)?;
        self.emit(range, Instruction::Throw { value: value_reg })?;
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // break
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_BREAK)?;
        if !self.route_through_finally(builder, range, COMPLETION_BREAK, None)? {
            let break_jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
            match self.loops.last_mut() {
                Some(frame) => frame.breaks.push(break_jump),
                None => {
                    let target = self.next_pc();
                    self.patch_jump(break_jump, target);
                }
            }
        }
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // continue
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_CONTINUE)?;
        if !self.route_through_finally(builder, range, COMPLETION_CONTINUE, None)? {
            let continue_jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
            match self.nearest_loop_index() {
                Some(index) => self.loops[index].continues.push(continue_jump),
                None => {
                    let target = self.next_pc();
                    self.patch_jump(continue_jump, target);
                }
            }
        }
        let after = self.next_pc();
        self.patch_jump(skip, after);
        Ok(())
    }

    /// Emits `if kind_reg != kind { jump skip }`, returning the skip jump to
    /// patch past the guarded completion.
    fn emit_kind_guard(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        kind_reg: Register,
        kind: i32,
    ) -> Result<Pc, LowerError> {
        let marker = self.load_constant(builder, Constant::Int32(kind), range)?;
        let matched = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Binary {
                dst: matched,
                op: BinaryOp::StrictEqual,
                left: kind_reg,
                right: marker,
            },
        )?;
        self.emit(
            range,
            Instruction::JumpIfFalse {
                condition: matched,
                target: Pc::new(0),
            },
        )
    }

    fn lower_variable_declaration(
        &mut self,
        builder: &mut ModuleBuilder,
        declaration: &VariableDeclaration,
    ) -> Result<(), LowerError> {
        let declaration_scope = match declaration.kind {
            VariableKind::Var => DeclarationScope::Function,
            VariableKind::Let | VariableKind::Const => DeclarationScope::Lexical,
            VariableKind::Using | VariableKind::AwaitUsing => {
                let range = declaration
                    .declarations
                    .first()
                    .map_or_else(zero_range, |declarator| declarator.range());
                return Err(self.unsupported(range, UnsupportedConstruct::UsingDeclaration));
            }
        };
        for declarator in &declaration.declarations {
            let range = declarator.range();
            let data = declarator.data();
            self.predeclare_captured_pattern(&data.binding, declaration_scope)?;
            let value = match &data.initializer {
                Some(initializer) => self.lower_expression(builder, initializer)?,
                None => {
                    // A bare declaration (`let x;`) binds undefined.
                    self.undefined(builder, range)?
                }
            };
            self.bind_pattern(builder, &data.binding, value, declaration_scope)?;
        }
        Ok(())
    }

    fn lower_iteration_variable_declaration(
        &mut self,
        builder: &mut ModuleBuilder,
        declaration: &VariableDeclaration,
    ) -> Result<(), LowerError> {
        debug_assert!(matches!(
            declaration.kind,
            VariableKind::Let | VariableKind::Const
        ));
        for declarator in &declaration.declarations {
            let range = declarator.range();
            let data = declarator.data();
            self.predeclare_captured_pattern(&data.binding, DeclarationScope::Iteration)?;
            let value = match &data.initializer {
                Some(initializer) => self.lower_expression(builder, initializer)?,
                None => self.undefined(builder, range)?,
            };
            self.bind_pattern(builder, &data.binding, value, DeclarationScope::Iteration)?;
        }
        Ok(())
    }

    fn instantiate_function_declaration(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        name: &str,
        site: BindingSite,
        function: &FunctionLike,
    ) -> Result<(), LowerError> {
        if function.body.is_none() {
            return Ok(());
        }
        self.predeclare_captured_binding(name, range, site, DeclarationScope::Function)?;
        let closure = self.build_constructible_function_value(
            builder,
            range,
            Some(name.to_owned()),
            function,
        )?;
        self.store_binding(
            builder,
            name,
            closure,
            range,
            site,
            DeclarationScope::Function,
        )
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    fn lower_expression(
        &mut self,
        builder: &mut ModuleBuilder,
        expression: &Expr,
    ) -> Result<Register, LowerError> {
        let range = expression.range();
        match expression.data() {
            Expression::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.read_name(builder, &name, range)
            }
            Expression::This => self.this_value(range),
            // Derived constructors lower their direct `super(...)` call with
            // the captured parent and existing receiver.
            Expression::Super => {
                Err(self.unsupported(range, UnsupportedConstruct::DerivedConstructorShape))
            }
            Expression::Literal(literal) => self.lower_literal(builder, range, literal),
            Expression::Template(template) => self.lower_template(builder, range, template),
            Expression::TaggedTemplate(tagged) => {
                self.lower_tagged_template(builder, range, tagged)
            }
            Expression::Array(array) => self.lower_array(builder, range, array),
            Expression::Object(object) => self.lower_object(builder, range, object),
            Expression::Function(function) => {
                self.build_constructible_function_value(builder, range, None, &function.function)
            }
            Expression::Class(class) => {
                let name = class
                    .class
                    .name
                    .as_ref()
                    .map(|identifier| {
                        self.identifier_text(identifier)
                            .map(|name| (name, binding_site(identifier.range())))
                    })
                    .transpose()?;
                self.lower_class_value(
                    builder,
                    range,
                    &class.class,
                    None,
                    name.as_ref().map(|(name, site)| (name.as_str(), *site)),
                )
            }
            Expression::Arrow(arrow) => self.lower_arrow(builder, range, arrow),
            Expression::Call(call) => self.lower_call(builder, range, call),
            Expression::Member(member) => {
                let (_, value) = self.lower_member(builder, range, member)?;
                Ok(value)
            }
            Expression::New(new) => self.lower_new(builder, range, new),
            Expression::Await(await_expression) => {
                self.lower_await(builder, range, await_expression)
            }
            Expression::Yield(yield_expression) => {
                self.lower_yield(builder, range, yield_expression)
            }
            Expression::Unary(unary) => self.lower_unary(builder, range, unary),
            Expression::Update(update) => self.lower_update(builder, range, update),
            Expression::Binary(binary) => self.lower_binary(builder, range, binary),
            Expression::Logical(logical) => self.lower_logical(builder, range, logical),
            Expression::Conditional(conditional) => {
                self.lower_conditional(builder, range, conditional)
            }
            Expression::Assignment(assignment) => self.lower_assignment(builder, range, assignment),
            Expression::Sequence(sequence) => {
                let mut last = None;
                for expression in &sequence.expressions {
                    last = Some(self.lower_expression(builder, expression)?);
                }
                match last {
                    Some(register) => Ok(register),
                    None => self.undefined(builder, range),
                }
            }
            Expression::Parenthesized(inner) => self.lower_expression(builder, inner),
            Expression::As(as_expression) => {
                self.lower_expression(builder, &as_expression.expression)
            }
            Expression::Satisfies(satisfies) => {
                self.lower_expression(builder, &satisfies.expression)
            }
            Expression::TypeAssertion(assertion) => {
                self.lower_expression(builder, &assertion.expression)
            }
            Expression::NonNull(non_null) => self.lower_expression(builder, &non_null.expression),
            Expression::Import(import) => self.lower_import_expression(builder, range, import),
            Expression::Meta(meta) => match meta {
                MetaProperty::NewTarget => self.new_target_value(range),
                MetaProperty::ImportMeta => {
                    Err(self.unsupported(range, UnsupportedConstruct::ImportMeta))
                }
            },
            Expression::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }

    fn lower_unary(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        unary: &crate::syntax::UnaryExpression,
    ) -> Result<Register, LowerError> {
        let op = match unary.operator {
            UnaryOperator::Void => {
                self.lower_expression(builder, &unary.argument)?;
                return self.undefined(builder, range);
            }
            UnaryOperator::Delete => return self.lower_delete(builder, range, &unary.argument),
            UnaryOperator::Typeof => return self.lower_typeof(builder, range, &unary.argument),
            UnaryOperator::Plus => UnaryOp::Plus,
            UnaryOperator::Minus => UnaryOp::Negate,
            UnaryOperator::Not => UnaryOp::LogicalNot,
            UnaryOperator::BitNot => UnaryOp::BitwiseNot,
        };
        let operand = self.lower_expression(builder, &unary.argument)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::Unary { dst, op, operand })?;
        Ok(dst)
    }

    /// `typeof x` never throws for a free/undeclared name, so a bare-identifier
    /// operand that resolves to the environment uses [`Instruction::TypeOfGlobal`].
    fn lower_typeof(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        argument: &Expr,
    ) -> Result<Register, LowerError> {
        if let Expression::Identifier(identifier) = argument.data() {
            let name = self.identifier_text(identifier)?;
            let resolved = self.resolve(&name).is_some()
                || (name == "arguments" && !matches!(self.arguments_source, ArgumentsSource::None))
                || name == "undefined";
            if !resolved {
                let id = builder.intern(Constant::String(EcmaString::from_utf8(&name)), range)?;
                let dst = self.alloc_register(range)?;
                self.emit(range, Instruction::TypeOfGlobal { dst, name: id })?;
                return Ok(dst);
            }
        }
        let operand = self.lower_expression(builder, argument)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Unary {
                dst,
                op: UnaryOp::TypeOf,
                operand,
            },
        )?;
        Ok(dst)
    }

    fn lower_delete(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        argument: &Expr,
    ) -> Result<Register, LowerError> {
        match argument.data() {
            Expression::Member(member) => {
                if member.optional {
                    return self.lower_optional_delete(builder, range, member);
                }
                let object = self.lower_expression(builder, &member.object)?;
                let key = self.member_key(builder, &member.property)?;
                let dst = self.alloc_register(range)?;
                self.emit(range, Instruction::DeleteProperty { dst, object, key })?;
                Ok(dst)
            }
            Expression::Parenthesized(inner) => self.lower_delete(builder, range, inner),
            // `delete x` of a binding is a no-op returning false in strict/module
            // code (bindings are non-configurable).
            _ => self.load_constant(builder, Constant::Boolean(false), range),
        }
    }

    fn lower_optional_delete(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &MemberExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let truthy = self.load_constant(builder, Constant::Boolean(true), range)?;
        self.move_to(range, result, truthy)?;
        let object = self.lower_expression(builder, &member.object)?;
        let is_nullish = self.compute_nullish(builder, range, object)?;
        let skip = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: is_nullish,
                target: Pc::new(0),
            },
        )?;
        let key = self.member_key(builder, &member.property)?;
        let deleted = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::DeleteProperty {
                dst: deleted,
                object,
                key,
            },
        )?;
        self.move_to(range, result, deleted)?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(result)
    }

    fn lower_binary(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        binary: &BinaryExpression,
    ) -> Result<Register, LowerError> {
        let op = map_binary_operator(binary.operator);
        let left = self.lower_expression(builder, &binary.left)?;
        let right = self.lower_expression(builder, &binary.right)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Binary {
                dst,
                op,
                left,
                right,
            },
        )?;
        Ok(dst)
    }

    fn lower_logical(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        logical: &LogicalExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let left = self.lower_expression(builder, &logical.left)?;
        self.move_to(range, result, left)?;
        let short_circuit = self.branch_on_short_circuit(builder, range, logical.operator, left)?;
        let right = self.lower_expression(builder, &logical.right)?;
        self.move_to(range, result, right)?;
        let end = self.next_pc();
        self.patch_jump(short_circuit, end);
        Ok(result)
    }

    /// Emits the branch that keeps the left operand of a short-circuit
    /// operator, returning the placeholder jump to patch to the merge.
    fn branch_on_short_circuit(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        operator: LogicalOperator,
        left: Register,
    ) -> Result<Pc, LowerError> {
        match operator {
            LogicalOperator::And => self.emit(
                range,
                Instruction::JumpIfFalse {
                    condition: left,
                    target: Pc::new(0),
                },
            ),
            LogicalOperator::Or => self.emit(
                range,
                Instruction::JumpIfTrue {
                    condition: left,
                    target: Pc::new(0),
                },
            ),
            LogicalOperator::Nullish => {
                let is_nullish = self.compute_nullish(builder, range, left)?;
                self.emit(
                    range,
                    Instruction::JumpIfFalse {
                        condition: is_nullish,
                        target: Pc::new(0),
                    },
                )
            }
        }
    }

    /// Computes whether `value` is `null` or `undefined` (`value == null`).
    fn compute_nullish(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        value: Register,
    ) -> Result<Register, LowerError> {
        let null = self.load_constant(builder, Constant::Null, range)?;
        let is_null = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Binary {
                dst: is_null,
                op: BinaryOp::Equal,
                left: value,
                right: null,
            },
        )?;
        Ok(is_null)
    }

    fn lower_conditional(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        conditional: &ConditionalExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let condition = self.lower_expression(builder, &conditional.test)?;
        let to_alternate = self.emit(
            range,
            Instruction::JumpIfFalse {
                condition,
                target: Pc::new(0),
            },
        )?;
        let consequent = self.lower_expression(builder, &conditional.consequent)?;
        self.move_to(range, result, consequent)?;
        let to_end = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let alternate_pc = self.next_pc();
        self.patch_jump(to_alternate, alternate_pc);
        let alternate = self.lower_expression(builder, &conditional.alternate)?;
        self.move_to(range, result, alternate)?;
        let end = self.next_pc();
        self.patch_jump(to_end, end);
        Ok(result)
    }

    // ------------------------------------------------------------------
    // Assignment and update
    // ------------------------------------------------------------------

    fn lower_assignment(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        assignment: &AssignmentExpression,
    ) -> Result<Register, LowerError> {
        match assignment.left.data() {
            AssignmentTarget::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.lower_identifier_assignment(builder, range, &name, assignment)
            }
            AssignmentTarget::Member(member) => {
                self.lower_member_assignment(builder, range, member, assignment)
            }
            AssignmentTarget::Object(_) | AssignmentTarget::Array(_) => {
                // Destructuring assignment applies only for `=`.
                if compound_operator(assignment.operator).is_some() {
                    return Err(
                        self.missing(assignment.left.range(), NodeKind::AssignmentExpression)
                    );
                }
                let value = self.lower_expression(builder, &assignment.right)?;
                self.assign_target(builder, &assignment.left, value)?;
                Ok(value)
            }
            AssignmentTarget::Missing(missing) => {
                Err(self.missing(assignment.left.range(), missing.expected()))
            }
        }
    }

    fn lower_identifier_assignment(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        name: &str,
        assignment: &AssignmentExpression,
    ) -> Result<Register, LowerError> {
        match compound_operator(assignment.operator) {
            None => {
                let value = self.lower_expression(builder, &assignment.right)?;
                self.assign_name(builder, name, value, range)?;
                Ok(value)
            }
            Some(CompoundOp::Arithmetic(op)) => {
                let current = self.read_name_value(builder, name, range)?;
                let right = self.lower_expression(builder, &assignment.right)?;
                let result = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Binary {
                        dst: result,
                        op,
                        left: current,
                        right,
                    },
                )?;
                self.assign_name(builder, name, result, range)?;
                Ok(result)
            }
            Some(CompoundOp::Logical(op)) => {
                let result = self.alloc_register(range)?;
                let current = self.read_name_value(builder, name, range)?;
                self.move_to(range, result, current)?;
                let skip = self.branch_on_short_circuit(builder, range, op, current)?;
                let value = self.lower_expression(builder, &assignment.right)?;
                self.assign_name(builder, name, value, range)?;
                self.move_to(range, result, value)?;
                let end = self.next_pc();
                self.patch_jump(skip, end);
                Ok(result)
            }
        }
    }

    fn lower_member_assignment(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &AssignmentMemberTarget,
        assignment: &AssignmentExpression,
    ) -> Result<Register, LowerError> {
        let object = self.lower_expression(builder, &member.object)?;
        let key = self.member_key(builder, &member.property)?;
        match compound_operator(assignment.operator) {
            None => {
                let value = self.lower_expression(builder, &assignment.right)?;
                self.emit(range, Instruction::SetProperty { object, key, value })?;
                Ok(value)
            }
            Some(CompoundOp::Arithmetic(op)) => {
                let current = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::GetProperty {
                        dst: current,
                        object,
                        key,
                    },
                )?;
                let right = self.lower_expression(builder, &assignment.right)?;
                let result = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Binary {
                        dst: result,
                        op,
                        left: current,
                        right,
                    },
                )?;
                self.emit(
                    range,
                    Instruction::SetProperty {
                        object,
                        key,
                        value: result,
                    },
                )?;
                Ok(result)
            }
            Some(CompoundOp::Logical(op)) => {
                let result = self.alloc_register(range)?;
                let current = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::GetProperty {
                        dst: current,
                        object,
                        key,
                    },
                )?;
                self.move_to(range, result, current)?;
                let skip = self.branch_on_short_circuit(builder, range, op, current)?;
                let value = self.lower_expression(builder, &assignment.right)?;
                self.emit(range, Instruction::SetProperty { object, key, value })?;
                self.move_to(range, result, value)?;
                let end = self.next_pc();
                self.patch_jump(skip, end);
                Ok(result)
            }
        }
    }

    fn lower_update(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        update: &UpdateExpression,
    ) -> Result<Register, LowerError> {
        let op = match update.operator {
            UpdateOperator::Increment => BinaryOp::Add,
            UpdateOperator::Decrement => BinaryOp::Subtract,
        };
        match update.argument.data() {
            AssignmentTarget::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                let current = self.read_name_value(builder, &name, range)?;
                let old = self.alloc_register(range)?;
                self.move_to(range, old, current)?;
                let one = self.load_constant(builder, Constant::Int32(1), range)?;
                let updated = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Binary {
                        dst: updated,
                        op,
                        left: old,
                        right: one,
                    },
                )?;
                self.assign_name(builder, &name, updated, range)?;
                Ok(if update.prefix { updated } else { old })
            }
            AssignmentTarget::Member(member) => {
                let object = self.lower_expression(builder, &member.object)?;
                let key = self.member_key(builder, &member.property)?;
                let old = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::GetProperty {
                        dst: old,
                        object,
                        key,
                    },
                )?;
                let one = self.load_constant(builder, Constant::Int32(1), range)?;
                let updated = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Binary {
                        dst: updated,
                        op,
                        left: old,
                        right: one,
                    },
                )?;
                self.emit(
                    range,
                    Instruction::SetProperty {
                        object,
                        key,
                        value: updated,
                    },
                )?;
                Ok(if update.prefix { updated } else { old })
            }
            AssignmentTarget::Object(_) | AssignmentTarget::Array(_) => {
                Err(self.missing(update.argument.range(), NodeKind::UpdateExpression))
            }
            AssignmentTarget::Missing(missing) => {
                Err(self.missing(update.argument.range(), missing.expected()))
            }
        }
    }

    /// `await x` suspends on `x` ([`Instruction::Await`], distinct from the
    /// `yield` form's [`Instruction::Suspend`]) and resumes with the settled
    /// value in `dst`.
    fn lower_await(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        await_expression: &AwaitExpression,
    ) -> Result<Register, LowerError> {
        let src = self.lower_expression(builder, &await_expression.argument)?;
        self.emit_await(range, src)
    }

    /// `yield x` yields `x` and resumes with the `.next(v)` value in `dst`;
    /// `yield* x` delegates to the iterator of `x`.
    fn lower_yield(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        yield_expression: &YieldExpression,
    ) -> Result<Register, LowerError> {
        if yield_expression.delegate {
            return self.lower_yield_delegate(builder, range, yield_expression);
        }
        let src = match &yield_expression.argument {
            Some(expression) => self.lower_expression(builder, expression)?,
            None => self.undefined(builder, range)?,
        };
        self.emit_suspend(range, src)
    }

    /// `yield* x`: iterate `x`, yielding each produced value; the expression's
    /// value is the delegate iterator's completion value.
    fn lower_yield_delegate(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        yield_expression: &YieldExpression,
    ) -> Result<Register, LowerError> {
        let subject = match &yield_expression.argument {
            Some(expression) => self.lower_expression(builder, expression)?,
            None => self.undefined(builder, range)?,
        };
        let iterator = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetIterator {
                dst: iterator,
                src: subject,
                kind: IteratorKind::Sync,
            },
        )?;
        let done = self.alloc_register(range)?;
        let value = self.alloc_register(range)?;
        let result = self.alloc_register(range)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, result, undefined)?;
        let head = self.next_pc();
        self.emit(
            range,
            Instruction::IteratorNext {
                done,
                value,
                iterator,
            },
        )?;
        let exit_jump = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: done,
                target: Pc::new(0),
            },
        )?;
        let resumed = self.emit_suspend(range, value)?;
        self.move_to(range, result, resumed)?;
        self.emit(range, Instruction::Jump { target: head })?;
        let exit = self.next_pc();
        self.patch_jump(exit_jump, exit);
        // On completion the delegate's final value is `value`.
        self.move_to(range, result, value)?;
        Ok(result)
    }

    /// Emits a `yield` suspension producing `src`, returning the register that
    /// receives the resumed value.
    fn emit_suspend(&mut self, range: TextRange, src: Register) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        let resume = Pc::new(self.code.len() as u32 + 1);
        self.emit(range, Instruction::Suspend { dst, src, resume })?;
        Ok(dst)
    }

    /// Emits an `await` suspension on `src`, returning the register that
    /// receives the settled value. Distinct from [`Self::emit_suspend`] so an
    /// async-generator body keeps `await` and `yield` apart.
    fn emit_await(&mut self, range: TextRange, src: Register) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        let resume = Pc::new(self.code.len() as u32 + 1);
        self.emit(range, Instruction::Await { dst, src, resume })?;
        Ok(dst)
    }

    // ------------------------------------------------------------------
    // Member access and calls
    // ------------------------------------------------------------------

    /// Lowers a member expression, returning `(object, value)` where `object`
    /// is the base register (the receiver for a following call) and `value` is
    /// the read property.
    fn lower_member(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &MemberExpression,
    ) -> Result<(Register, Register), LowerError> {
        if member.optional {
            let value = self.lower_optional_chain(builder, range, member)?;
            // The receiver of an optional member read is the (possibly nullish)
            // base; a call through it re-evaluates, so return undefined here.
            let object = self.undefined(builder, range)?;
            return Ok((object, value));
        }
        let object = self.lower_expression(builder, &member.object)?;
        let key = self.member_key(builder, &member.property)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::GetProperty { dst, object, key })?;
        Ok((object, dst))
    }

    /// Lowers an optional member/call chain node `a?.b` to a short-circuiting
    /// read whose result is `undefined` when the base is nullish.
    fn lower_optional_chain(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &MemberExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, result, undefined)?;
        let object = self.lower_expression(builder, &member.object)?;
        let is_nullish = self.compute_nullish(builder, range, object)?;
        let skip = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: is_nullish,
                target: Pc::new(0),
            },
        )?;
        let key = self.member_key(builder, &member.property)?;
        let value = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetProperty {
                dst: value,
                object,
                key,
            },
        )?;
        self.move_to(range, result, value)?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(result)
    }

    fn lower_call(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        call: &CallExpression,
    ) -> Result<Register, LowerError> {
        if let Expression::Member(member) = call.callee.data()
            && member.optional
        {
            return self.lower_optional_member_call(builder, range, call, member);
        }
        if call.optional {
            return self.lower_optional_call(builder, range, call);
        }
        let (callee, this_value) = self.lower_callee(builder, range, &call.callee)?;
        let arguments = self.build_arguments(builder, range, &call.arguments)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst,
                callee,
                this_value,
                arguments,
            },
        )?;
        Ok(dst)
    }

    fn lower_optional_member_call(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        call: &CallExpression,
        member: &MemberExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, result, undefined)?;

        let object = self.lower_expression(builder, &member.object)?;
        let object_is_nullish = self.compute_nullish(builder, range, object)?;
        let object_skip = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: object_is_nullish,
                target: Pc::new(0),
            },
        )?;

        let key = self.member_key(builder, &member.property)?;
        let callee = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetProperty {
                dst: callee,
                object,
                key,
            },
        )?;

        let callee_skip = if call.optional {
            let callee_is_nullish = self.compute_nullish(builder, range, callee)?;
            Some(self.emit(
                range,
                Instruction::JumpIfTrue {
                    condition: callee_is_nullish,
                    target: Pc::new(0),
                },
            )?)
        } else {
            None
        };

        let arguments = self.build_arguments(builder, range, &call.arguments)?;
        let value = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst: value,
                callee,
                this_value: object,
                arguments,
            },
        )?;
        self.move_to(range, result, value)?;

        let end = self.next_pc();
        self.patch_jump(object_skip, end);
        if let Some(callee_skip) = callee_skip {
            self.patch_jump(callee_skip, end);
        }
        Ok(result)
    }

    /// Evaluates a call's callee, returning `(callee, this_value)`. A member
    /// callee `obj.m()` uses `obj` as the receiver; a `super.m()` uses `this`.
    fn lower_callee(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        callee: &Expr,
    ) -> Result<(Register, Register), LowerError> {
        match callee.data() {
            Expression::Member(member) if !member.optional => {
                if matches!(member.object.data(), Expression::Super) {
                    // `super.m()`: read from the prototype chain, call with `this`.
                    let this_value = self.this_value(range)?;
                    let object = this_value;
                    let key = self.member_key(builder, &member.property)?;
                    let value = self.alloc_register(range)?;
                    self.emit(
                        range,
                        Instruction::GetProperty {
                            dst: value,
                            object,
                            key,
                        },
                    )?;
                    return Ok((value, this_value));
                }
                let (object, value) = self.lower_member(builder, callee.range(), member)?;
                Ok((value, object))
            }
            Expression::Super => {
                // `super(...)`: invoke the parent constructor with the current
                // `this`. The parent constructor is read from the environment
                // binding created for the class's `extends` clause is not
                // available here, so route through the receiver's prototype
                // constructor via `this`.
                let this_value = self.this_value(range)?;
                Ok((this_value, this_value))
            }
            _ => {
                let callee = self.lower_expression(builder, callee)?;
                let this_value = self.undefined(builder, range)?;
                Ok((callee, this_value))
            }
        }
    }

    fn lower_optional_call(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        call: &CallExpression,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, result, undefined)?;
        let (callee, this_value) = self.lower_callee(builder, range, &call.callee)?;
        let is_nullish = self.compute_nullish(builder, range, callee)?;
        let skip = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: is_nullish,
                target: Pc::new(0),
            },
        )?;
        let arguments = self.build_arguments(builder, range, &call.arguments)?;
        let value = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst: value,
                callee,
                this_value,
                arguments,
            },
        )?;
        self.move_to(range, result, value)?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(result)
    }

    fn lower_new(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        new: &NewExpression,
    ) -> Result<Register, LowerError> {
        let callee = self.lower_expression(builder, &new.callee)?;
        let arguments = self.build_arguments(builder, range, &new.arguments)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Construct {
                dst,
                callee,
                arguments,
            },
        )?;
        Ok(dst)
    }

    /// Builds one dynamic arguments array from positional arguments and spreads.
    fn build_arguments(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        arguments: &[CallArgument],
    ) -> Result<Register, LowerError> {
        let array = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: array })?;
        for argument in arguments {
            match argument {
                CallArgument::Expression(expression) => {
                    let value = self.lower_expression(builder, expression)?;
                    self.emit(range, Instruction::ArrayPush { array, value })?;
                }
                CallArgument::Spread(spread) => {
                    let iterable = self.lower_expression(builder, &spread.argument)?;
                    self.emit(range, Instruction::ArrayExtend { array, iterable })?;
                }
                CallArgument::Missing(missing) => {
                    return Err(self.error(
                        zero_range(),
                        LowerErrorKind::MissingSyntax {
                            expected: missing.expected(),
                        },
                    ));
                }
            }
        }
        Ok(array)
    }

    // ------------------------------------------------------------------
    // Arrays and objects
    // ------------------------------------------------------------------

    fn lower_array(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        array: &crate::syntax::ArrayLiteral,
    ) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst })?;
        for element in &array.elements {
            match element {
                ArrayElement::Expression(expression) => {
                    let value = self.lower_expression(builder, expression)?;
                    self.emit(range, Instruction::ArrayPush { array: dst, value })?;
                }
                ArrayElement::Spread(spread) => {
                    let iterable = self.lower_expression(builder, &spread.argument)?;
                    self.emit(
                        range,
                        Instruction::ArrayExtend {
                            array: dst,
                            iterable,
                        },
                    )?;
                }
                ArrayElement::Elision => {
                    let hole = self.undefined(builder, range)?;
                    self.emit(
                        range,
                        Instruction::ArrayPush {
                            array: dst,
                            value: hole,
                        },
                    )?;
                }
                ArrayElement::Missing(missing) => {
                    return Err(self.missing(range, missing.expected()));
                }
            }
        }
        Ok(dst)
    }

    fn lower_object(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        object: &ObjectLiteral,
    ) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateObject { dst })?;
        for member in &object.members {
            match member.data() {
                ObjectMember::Property(property) => {
                    let key = self.property_key(builder, &property.name)?;
                    let value = self.lower_expression(builder, &property.value)?;
                    self.install_property(
                        builder,
                        member.range(),
                        dst,
                        key,
                        value,
                        property.modifier,
                    )?;
                }
                ObjectMember::Method(method) => {
                    let key = self.property_key(builder, &method.name)?;
                    let value =
                        self.build_function_value(builder, member.range(), None, &method.function)?;
                    self.install_property(
                        builder,
                        member.range(),
                        dst,
                        key,
                        value,
                        method.modifier,
                    )?;
                }
                ObjectMember::Spread(spread) => {
                    let source = self.lower_expression(builder, &spread.argument)?;
                    self.emit(
                        member.range(),
                        Instruction::ObjectSpread {
                            target: dst,
                            source,
                        },
                    )?;
                }
                ObjectMember::Missing(missing) => {
                    return Err(self.missing(member.range(), missing.expected()));
                }
            }
        }
        Ok(dst)
    }

    /// Installs a data property, getter, or setter under `key` on `object`.
    fn install_property(
        &mut self,
        _builder: &mut ModuleBuilder,
        range: TextRange,
        object: Register,
        key: Register,
        value: Register,
        modifier: PropertyModifier,
    ) -> Result<(), LowerError> {
        match modifier {
            PropertyModifier::None => {
                self.emit(range, Instruction::SetProperty { object, key, value })?;
            }
            PropertyModifier::Get => {
                self.emit(
                    range,
                    Instruction::DefineAccessor {
                        object,
                        key,
                        accessor: value,
                        kind: AccessorKind::Getter,
                    },
                )?;
            }
            PropertyModifier::Set => {
                self.emit(
                    range,
                    Instruction::DefineAccessor {
                        object,
                        key,
                        accessor: value,
                        kind: AccessorKind::Setter,
                    },
                )?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Literals, templates, regex
    // ------------------------------------------------------------------

    fn lower_literal(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        literal: &Literal,
    ) -> Result<Register, LowerError> {
        match literal {
            Literal::Number(number) => self.lower_numeric_literal(builder, number),
            Literal::String(string) => {
                let value = self.string_literal_value(string)?;
                self.string_reg(builder, value, range)
            }
            Literal::Boolean(boolean) => {
                let value = self.boolean_literal_value(boolean)?;
                self.load_constant(builder, Constant::Boolean(value), range)
            }
            Literal::Null(_) => self.load_constant(builder, Constant::Null, range),
            Literal::BigInt(_) => self.lower_bigint_literal(builder, range, literal),
            Literal::Regex(regex) => self.lower_regex_literal(builder, range, regex),
        }
    }

    fn lower_template(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        template: &TemplateLiteral,
    ) -> Result<Register, LowerError> {
        // `` `a${x}b` `` == "a" + x + "b": the leading string operand forces
        // the whole chain to string concatenation.
        let cooked = self.cooked_template_parts(template)?;
        let first = cooked.first().cloned().unwrap_or_default();
        let mut acc = self.string_reg(builder, first, range)?;
        for (index, expression) in template.expressions.iter().enumerate() {
            let value = self.lower_expression(builder, expression)?;
            let joined = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::Binary {
                    dst: joined,
                    op: BinaryOp::Add,
                    left: acc,
                    right: value,
                },
            )?;
            acc = joined;
            let chunk = cooked.get(index + 1).cloned().unwrap_or_default();
            let chunk_reg = self.string_reg(builder, chunk, range)?;
            let joined = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::Binary {
                    dst: joined,
                    op: BinaryOp::Add,
                    left: acc,
                    right: chunk_reg,
                },
            )?;
            acc = joined;
        }
        Ok(acc)
    }

    /// `tag`...`` calls `tag` with a cooked strings array (carrying a `raw`
    /// property) followed by the substitution values.
    fn lower_tagged_template(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        tagged: &crate::syntax::TaggedTemplateExpression,
    ) -> Result<Register, LowerError> {
        let (callee, this_value) = self.lower_callee(builder, range, &tagged.tag)?;
        let cooked = self.cooked_template_parts(&tagged.template)?;
        let raw = self.raw_template_parts(&tagged.template)?;
        // strings array (cooked) with a `.raw` array.
        let strings = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: strings })?;
        for part in &cooked {
            let value = self.string_reg(builder, part.clone(), range)?;
            self.emit(
                range,
                Instruction::ArrayPush {
                    array: strings,
                    value,
                },
            )?;
        }
        let raw_array = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: raw_array })?;
        for part in &raw {
            let value = self.string_reg(builder, part.clone(), range)?;
            self.emit(
                range,
                Instruction::ArrayPush {
                    array: raw_array,
                    value,
                },
            )?;
        }
        let raw_key = self.string_reg(builder, EcmaString::from_utf8("raw"), range)?;
        self.emit(
            range,
            Instruction::SetProperty {
                object: strings,
                key: raw_key,
                value: raw_array,
            },
        )?;
        let arguments = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: arguments })?;
        self.emit(
            range,
            Instruction::ArrayPush {
                array: arguments,
                value: strings,
            },
        )?;
        for expression in &tagged.template.expressions {
            let value = self.lower_expression(builder, expression)?;
            self.emit(
                range,
                Instruction::ArrayPush {
                    array: arguments,
                    value,
                },
            )?;
        }
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst,
                callee,
                this_value,
                arguments,
            },
        )?;
        Ok(dst)
    }

    fn cooked_template_parts(
        &self,
        template: &TemplateLiteral,
    ) -> Result<Vec<EcmaString>, LowerError> {
        template
            .elements
            .iter()
            .map(|element| self.template_element_text(element, true))
            .collect()
    }

    fn raw_template_parts(
        &self,
        template: &TemplateLiteral,
    ) -> Result<Vec<EcmaString>, LowerError> {
        template
            .elements
            .iter()
            .map(|element| self.template_element_text(element, false))
            .collect()
    }

    /// Extracts the interior text of a template element, cooking escapes when
    /// `cook` is set.
    fn template_element_text(
        &self,
        element: &TemplateElementNode,
        cook: bool,
    ) -> Result<EcmaString, LowerError> {
        let token = element.data().token();
        if token.is_missing() {
            return Ok(EcmaString::default());
        }
        let Some(text) = self.file.token_text(token) else {
            return Ok(EcmaString::default());
        };
        let interior = trim_template_delimiters(text, token.kind());
        if cook {
            Ok(cook_escapes(interior))
        } else {
            Ok(EcmaString::from_utf8(interior))
        }
    }

    fn lower_regex_literal(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        regex: &RegexLiteralNode,
    ) -> Result<Register, LowerError> {
        let token = regex.data().token();
        if token.is_missing() {
            return Err(self.missing(range, NodeKind::RegexLiteral));
        }
        let lexeme = self
            .file
            .token_text(token)
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidRegexLiteral))?;
        let (pattern, flags) = split_regex(lexeme)
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidRegexLiteral))?;
        let pattern_id =
            builder.intern(Constant::String(EcmaString::from_utf8(&pattern)), range)?;
        let flags_id = builder.intern(Constant::String(EcmaString::from_utf8(&flags)), range)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::CreateRegExp {
                dst,
                pattern: pattern_id,
                flags: flags_id,
            },
        )?;
        Ok(dst)
    }

    fn lower_bigint_literal(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        literal: &Literal,
    ) -> Result<Register, LowerError> {
        let Literal::BigInt(node) = literal else {
            unreachable!("lower_bigint_literal only handles bigint literals");
        };
        let token = node.data().token();
        if token.is_missing() {
            return Err(self.missing(range, NodeKind::BigIntLiteral));
        }
        let lexeme = self
            .file
            .token_text(token)
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidBigIntLiteral))?;
        let canonical = canonical_bigint_text(
            lexeme,
            MAX_BIGINT_BYTES as usize,
            MAX_BIGINT_CONVERSION_LIMB_OPS,
        )
        .map_err(|error| {
            self.error(
                range,
                match error {
                    BigIntTextError::Invalid => LowerErrorKind::InvalidBigIntLiteral,
                    BigIntTextError::Bytes => LowerErrorKind::Capacity(CapacityLimit::BigIntBytes),
                    BigIntTextError::Work => LowerErrorKind::Capacity(CapacityLimit::BigIntWork),
                },
            )
        })?;
        let value = BigIntLiteral::new(canonical)
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidBigIntLiteral))?;
        self.load_constant(builder, Constant::BigInt(value), range)
    }

    fn lower_numeric_literal(
        &mut self,
        builder: &mut ModuleBuilder,
        number: &NumericLiteralNode,
    ) -> Result<Register, LowerError> {
        let range = number.range();
        let token = number.data().token();
        if token.is_missing() {
            return Err(self.missing(range, NodeKind::NumericLiteral));
        }
        let lexeme = self
            .file
            .token_text(token)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidNumericLiteral))?;
        let value = cook_number(lexeme)
            .ok_or_else(|| self.error(range, LowerErrorKind::InvalidNumericLiteral))?;
        self.load_constant(builder, number_constant(value), range)
    }

    fn string_literal_value(&self, string: &StringLiteralNode) -> Result<EcmaString, LowerError> {
        let range = string.range();
        let token = string.data().token();
        let missing = || self.missing(range, NodeKind::StringLiteral);
        if token.is_missing() {
            return Err(missing());
        }
        let text = self.file.token_text(token).ok_or_else(missing)?;
        if text.len() < 2 {
            return Err(missing());
        }
        let interior = &text[1..text.len() - 1];
        Ok(cook_escapes(interior))
    }

    fn boolean_literal_value(&self, boolean: &BooleanLiteralNode) -> Result<bool, LowerError> {
        let token = boolean.data().token();
        match token.kind() {
            TokenKind::KwTrue if !token.is_missing() => Ok(true),
            TokenKind::KwFalse if !token.is_missing() => Ok(false),
            _ => Err(self.missing(boolean.range(), NodeKind::BooleanLiteral)),
        }
    }

    // ------------------------------------------------------------------
    // Property keys
    // ------------------------------------------------------------------

    /// A member-access key as a register (string, computed value, or private
    /// name binding).
    fn member_key(
        &mut self,
        builder: &mut ModuleBuilder,
        property: &MemberProperty,
    ) -> Result<Register, LowerError> {
        match property {
            MemberProperty::Named(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.string_reg(builder, EcmaString::from_utf8(&name), identifier.range())
            }
            MemberProperty::Computed(expression) => self.lower_expression(builder, expression),
            MemberProperty::Private(private) => {
                let name = self.private_text(private)?;
                self.read_name(builder, &name, private.range())
            }
        }
    }

    /// An object-literal / class-member property key as a register.
    fn property_key(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &PropertyName,
    ) -> Result<Register, LowerError> {
        match name {
            PropertyName::Identifier(identifier) => {
                let text = self.identifier_text(identifier)?;
                self.string_reg(builder, EcmaString::from_utf8(&text), identifier.range())
            }
            PropertyName::String(string) => {
                let value = self.string_literal_value(string)?;
                self.string_reg(builder, value, string.range())
            }
            PropertyName::Number(number) => {
                let key = numeric_key_text(self, number)?;
                self.string_reg(builder, EcmaString::from_utf8(&key), number.range())
            }
            PropertyName::Computed(expression) => self.lower_expression(builder, expression),
            PropertyName::Private(private) => {
                let name = self.private_text(private)?;
                self.read_name(builder, &name, private.range())
            }
            PropertyName::Missing(missing) => Err(self.error(
                zero_range(),
                LowerErrorKind::MissingSyntax {
                    expected: missing.expected(),
                },
            )),
        }
    }

    // ------------------------------------------------------------------
    // Modules
    // ------------------------------------------------------------------

    fn lower_import(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        import: &ImportDeclaration,
    ) -> Result<(), LowerError> {
        if self.goal == LoweringGoal::ClassicScript {
            return Err(self.unsupported(range, UnsupportedConstruct::ImportDeclarationInScript));
        }
        if import.type_only || self.goal == LoweringGoal::ProgramModule {
            return Ok(());
        }
        let specifier = self.string_literal_value(&import.source)?;
        let specifier_id = builder.intern(Constant::String(specifier), range)?;
        let module = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Import {
                dst: module,
                specifier: specifier_id,
            },
        )?;
        let Some(clause) = &import.clause else {
            return Ok(());
        };
        if let Some(default) = &clause.default {
            let name = self.identifier_text(default)?;
            let value = self.get_named(builder, range, module, "default")?;
            self.store_binding(
                builder,
                &name,
                value,
                range,
                binding_site(default.range()),
                DeclarationScope::Function,
            )?;
        }
        match &clause.binding {
            Some(ImportBinding::Namespace(identifier)) => {
                let name = self.identifier_text(identifier)?;
                self.store_binding(
                    builder,
                    &name,
                    module,
                    range,
                    binding_site(identifier.range()),
                    DeclarationScope::Function,
                )?;
            }
            Some(ImportBinding::Named(specifiers)) => {
                for specifier in specifiers {
                    let data = specifier.data();
                    if matches!(data.mode, ImportSpecifierMode::TypeOnly) {
                        continue;
                    }
                    let local = self.identifier_text(&data.local)?;
                    let imported = self.module_export_name(&data.imported)?;
                    let value = self.get_named(builder, range, module, &imported)?;
                    self.store_binding(
                        builder,
                        &local,
                        value,
                        range,
                        binding_site(data.local.range()),
                        DeclarationScope::Function,
                    )?;
                }
            }
            None => {}
        }
        Ok(())
    }

    fn lower_import_expression(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        import: &crate::syntax::ImportExpression,
    ) -> Result<Register, LowerError> {
        if self.goal == LoweringGoal::ClassicScript {
            return Err(self.unsupported(range, UnsupportedConstruct::DynamicImportInScript));
        }
        if let Expression::Literal(Literal::String(string)) = import.source.data() {
            let specifier = self.string_literal_value(string)?;
            let specifier_id = builder.intern(Constant::String(specifier), range)?;
            let dst = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::Import {
                    dst,
                    specifier: specifier_id,
                },
            )?;
            Ok(dst)
        } else {
            Err(self.unsupported(range, UnsupportedConstruct::DynamicImportExpression))
        }
    }

    fn get_named(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        object: Register,
        name: &str,
    ) -> Result<Register, LowerError> {
        let key = self.string_reg(builder, EcmaString::from_utf8(name), range)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::GetProperty { dst, object, key })?;
        Ok(dst)
    }

    /// Emits a runtime export of the local binding `name` under `exported`.
    fn export_binding(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        local: &str,
        exported: &str,
    ) -> Result<(), LowerError> {
        if self.goal == LoweringGoal::ProgramModule {
            return Ok(());
        }
        let src = self.read_name(builder, local, range)?;
        let name = builder.intern(Constant::String(EcmaString::from_utf8(exported)), range)?;
        self.emit(range, Instruction::Export { name, src })?;
        Ok(())
    }

    fn export_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        exported: &str,
        src: Register,
    ) -> Result<(), LowerError> {
        if self.goal == LoweringGoal::ProgramModule {
            debug_assert_eq!(exported, "default");
            return self.store_binding(
                builder,
                "*default*",
                src,
                range,
                binding_site(range),
                DeclarationScope::Lexical,
            );
        }
        let name = builder.intern(Constant::String(EcmaString::from_utf8(exported)), range)?;
        self.emit(range, Instruction::Export { name, src })?;
        Ok(())
    }

    fn lower_export(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        export: &ExportDeclaration,
    ) -> Result<(), LowerError> {
        if self.goal == LoweringGoal::ClassicScript {
            return Err(self.unsupported(range, UnsupportedConstruct::ExportDeclarationInScript));
        }

        match export {
            ExportDeclaration::Named(ExportNamedDeclaration::Declaration(statement)) => {
                self.lower_statement(builder, statement)?;
                for name in declared_names(self.file, statement) {
                    self.export_binding(builder, range, &name, &name)?;
                }
                Ok(())
            }
            ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                type_only,
                specifiers,
                source,
                ..
            }) => {
                if *type_only || self.goal == LoweringGoal::ProgramModule {
                    return Ok(());
                }
                if let Some(source) = source {
                    // Re-export from another module.
                    let specifier = self.string_literal_value(source)?;
                    let specifier_id = builder.intern(Constant::String(specifier), range)?;
                    let module = self.alloc_register(range)?;
                    self.emit(
                        range,
                        Instruction::Import {
                            dst: module,
                            specifier: specifier_id,
                        },
                    )?;
                    for specifier in specifiers {
                        let data = specifier.data();
                        if matches!(data.mode, ExportSpecifierMode::TypeOnly) {
                            continue;
                        }
                        let local = self.module_export_name(&data.local)?;
                        let exported = self.module_export_name(&data.exported)?;
                        let value = self.get_named(builder, range, module, &local)?;
                        self.export_value(builder, range, &exported, value)?;
                    }
                    return Ok(());
                }
                for specifier in specifiers {
                    let data = specifier.data();
                    if matches!(data.mode, ExportSpecifierMode::TypeOnly) {
                        continue;
                    }
                    let local = self.module_export_name(&data.local)?;
                    let exported = self.module_export_name(&data.exported)?;
                    self.export_binding(builder, range, &local, &exported)?;
                }
                Ok(())
            }
            ExportDeclaration::All(all) => {
                if all.type_only || self.goal == LoweringGoal::ProgramModule {
                    Ok(())
                } else {
                    Err(self.unsupported(range, UnsupportedConstruct::RuntimeExportAll))
                }
            }
            ExportDeclaration::Default(default) => match &default.value {
                ExportDefaultValue::Expression(expression) => {
                    let value = self.lower_expression(builder, expression)?;
                    self.export_value(builder, range, "default", value)
                }
                ExportDefaultValue::Function(function) => {
                    if function.body.is_none() {
                        return Ok(());
                    }
                    if let Some(identifier) = &function.name {
                        let name = self.identifier_text(identifier)?;
                        let closure = self.read_name(builder, &name, range)?;
                        self.export_value(builder, range, "default", closure)
                    } else {
                        let closure = self
                            .build_constructible_function_value(builder, range, None, function)?;
                        self.export_value(builder, range, "default", closure)
                    }
                }
                ExportDefaultValue::Class(class) => {
                    let value = self.lower_class_value(builder, range, class, None, None)?;
                    if let Some(identifier) = &class.name {
                        let name = self.identifier_text(identifier)?;
                        self.store_binding(
                            builder,
                            &name,
                            value,
                            range,
                            binding_site(identifier.range()),
                            DeclarationScope::Lexical,
                        )?;
                    }
                    self.export_value(builder, range, "default", value)
                }
                ExportDefaultValue::Missing(missing) => {
                    Err(self.missing(range, missing.expected()))
                }
            },
            ExportDeclaration::Assignment(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::ExportAssignment))
            }
        }
    }

    fn module_export_name(&self, name: &ModuleExportName) -> Result<String, LowerError> {
        match name {
            ModuleExportName::Identifier(identifier) => self.identifier_text(identifier),
            ModuleExportName::String(string) => self
                .string_literal_value(string)?
                .to_utf8_strict()
                .map_err(|_| self.error(string.range(), LowerErrorKind::IllFormedMetadataString)),
            ModuleExportName::Missing(missing) => Err(self.error(
                zero_range(),
                LowerErrorKind::MissingSyntax {
                    expected: missing.expected(),
                },
            )),
        }
    }

    // ------------------------------------------------------------------
    // Destructuring (binding and assignment)
    // ------------------------------------------------------------------

    /// Binds a binding pattern to `value`, declaring each identifier binding.
    fn bind_pattern(
        &mut self,
        builder: &mut ModuleBuilder,
        pattern: &Pattern,
        value: Register,
        declaration_scope: DeclarationScope,
    ) -> Result<(), LowerError> {
        let range = pattern.range();
        match pattern.data() {
            BindingPattern::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.store_binding(
                    builder,
                    &name,
                    value,
                    range,
                    binding_site(identifier.range()),
                    declaration_scope,
                )
            }
            BindingPattern::Object(object) => {
                let mut taken: Vec<Register> = Vec::new();
                for property in &object.properties {
                    if let BindingPattern::Rest(rest) = property.binding.data() {
                        let rest_value = self.rest_object(builder, range, value, &taken)?;
                        self.bind_pattern(builder, &rest.argument, rest_value, declaration_scope)?;
                        continue;
                    }
                    let key = self.property_key(builder, &property.name)?;
                    taken.push(key);
                    let element = self.alloc_register(range)?;
                    self.emit(
                        range,
                        Instruction::GetProperty {
                            dst: element,
                            object: value,
                            key,
                        },
                    )?;
                    let element = match &property.initializer {
                        Some(default) => self.apply_default(builder, range, element, default)?,
                        None => element,
                    };
                    self.bind_pattern(builder, &property.binding, element, declaration_scope)?;
                }
                Ok(())
            }
            BindingPattern::Array(array) => {
                let iterator = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::GetIterator {
                        dst: iterator,
                        src: value,
                        kind: IteratorKind::Sync,
                    },
                )?;
                for element in &array.elements {
                    match element {
                        ArrayBindingElement::Elision => {
                            self.iterator_step_discard(range, iterator)?;
                        }
                        ArrayBindingElement::Binding(inner) => {
                            if let BindingPattern::Rest(rest) = inner.data() {
                                let rest_value = self.rest_array(builder, range, iterator)?;
                                self.bind_pattern(
                                    builder,
                                    &rest.argument,
                                    rest_value,
                                    declaration_scope,
                                )?;
                            } else {
                                let (element_value, default) = self.destructure_element(inner);
                                let value = self.iterator_step_value(builder, range, iterator)?;
                                let value = match default {
                                    Some(default) => {
                                        self.apply_default(builder, range, value, default)?
                                    }
                                    None => value,
                                };
                                let _ = element_value;
                                self.bind_pattern(builder, inner, value, declaration_scope)?;
                            }
                        }
                        ArrayBindingElement::Missing(missing) => {
                            return Err(self.missing(range, missing.expected()));
                        }
                    }
                }
                Ok(())
            }
            BindingPattern::Assignment(assignment) => {
                let value = self.apply_default(builder, range, value, &assignment.right)?;
                self.bind_pattern(builder, &assignment.left, value, declaration_scope)
            }
            BindingPattern::Rest(rest) => {
                // A bare rest at the top level binds the whole value.
                self.bind_pattern(builder, &rest.argument, value, declaration_scope)
            }
            BindingPattern::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }

    /// Splits an assignment binding element into (inner pattern, default).
    fn destructure_element<'p>(&self, pattern: &'p Pattern) -> (&'p Pattern, Option<&'p Expr>) {
        if let BindingPattern::Assignment(assignment) = pattern.data() {
            (&assignment.left, Some(&assignment.right))
        } else {
            (pattern, None)
        }
    }

    /// `value === undefined ? default : value`.
    fn apply_default(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        value: Register,
        default: &Expr,
    ) -> Result<Register, LowerError> {
        let result = self.alloc_register(range)?;
        self.move_to(range, result, value)?;
        let undefined = self.undefined(builder, range)?;
        let is_undefined = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Binary {
                dst: is_undefined,
                op: BinaryOp::StrictEqual,
                left: value,
                right: undefined,
            },
        )?;
        let skip = self.emit(
            range,
            Instruction::JumpIfFalse {
                condition: is_undefined,
                target: Pc::new(0),
            },
        )?;
        let default_value = self.lower_expression(builder, default)?;
        self.move_to(range, result, default_value)?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(result)
    }

    /// Steps an iterator once, discarding the produced value.
    fn iterator_step_discard(
        &mut self,
        range: TextRange,
        iterator: Register,
    ) -> Result<(), LowerError> {
        let done = self.alloc_register(range)?;
        let value = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::IteratorNext {
                done,
                value,
                iterator,
            },
        )?;
        Ok(())
    }

    /// Steps an iterator, returning a register that holds the produced value
    /// (or `undefined` when the iterator is exhausted).
    fn iterator_step_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        iterator: Register,
    ) -> Result<Register, LowerError> {
        let done = self.alloc_register(range)?;
        let value = self.alloc_register(range)?;
        let result = self.alloc_register(range)?;
        let undefined = self.undefined(builder, range)?;
        self.move_to(range, result, undefined)?;
        self.emit(
            range,
            Instruction::IteratorNext {
                done,
                value,
                iterator,
            },
        )?;
        let skip = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: done,
                target: Pc::new(0),
            },
        )?;
        self.move_to(range, result, value)?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(result)
    }

    /// Collects the remaining iterator elements into a fresh array.
    fn rest_array(
        &mut self,
        _builder: &mut ModuleBuilder,
        range: TextRange,
        iterator: Register,
    ) -> Result<Register, LowerError> {
        let array = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: array })?;
        let done = self.alloc_register(range)?;
        let value = self.alloc_register(range)?;
        let head = self.next_pc();
        self.emit(
            range,
            Instruction::IteratorNext {
                done,
                value,
                iterator,
            },
        )?;
        let exit = self.emit(
            range,
            Instruction::JumpIfTrue {
                condition: done,
                target: Pc::new(0),
            },
        )?;
        self.emit(range, Instruction::ArrayPush { array, value })?;
        self.emit(range, Instruction::Jump { target: head })?;
        let exit_pc = self.next_pc();
        self.patch_jump(exit, exit_pc);
        Ok(array)
    }

    /// Builds `{ ...object }` minus the already-taken keys, for object rest.
    fn rest_object(
        &mut self,
        _builder: &mut ModuleBuilder,
        range: TextRange,
        object: Register,
        taken: &[Register],
    ) -> Result<Register, LowerError> {
        let rest = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateObject { dst: rest })?;
        self.emit(
            range,
            Instruction::ObjectSpread {
                target: rest,
                source: object,
            },
        )?;
        for key in taken {
            let discarded = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::DeleteProperty {
                    dst: discarded,
                    object: rest,
                    key: *key,
                },
            )?;
        }
        Ok(rest)
    }

    /// Assigns `value` into an existing assignment target (identifier, member,
    /// or nested destructuring pattern).
    fn assign_target(
        &mut self,
        builder: &mut ModuleBuilder,
        target: &AssignmentTargetNode,
        value: Register,
    ) -> Result<(), LowerError> {
        let range = target.range();
        match target.data() {
            AssignmentTarget::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.assign_name(builder, &name, value, range)
            }
            AssignmentTarget::Member(member) => {
                let object = self.lower_expression(builder, &member.object)?;
                let key = self.member_key(builder, &member.property)?;
                self.emit(range, Instruction::SetProperty { object, key, value })?;
                Ok(())
            }
            AssignmentTarget::Object(object) => {
                let mut taken: Vec<Register> = Vec::new();
                for property in &object.properties {
                    self.assign_object_property(builder, range, value, property, &mut taken)?;
                }
                Ok(())
            }
            AssignmentTarget::Array(array) => {
                let iterator = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::GetIterator {
                        dst: iterator,
                        src: value,
                        kind: IteratorKind::Sync,
                    },
                )?;
                for element in &array.elements {
                    match element {
                        AssignmentArrayElement::Elision => {
                            self.iterator_step_discard(range, iterator)?;
                        }
                        AssignmentArrayElement::Target(inner) => {
                            let element = self.iterator_step_value(builder, range, iterator)?;
                            self.assign_target(builder, inner, element)?;
                        }
                        AssignmentArrayElement::Missing(missing) => {
                            return Err(self.missing(range, missing.expected()));
                        }
                    }
                }
                Ok(())
            }
            AssignmentTarget::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }

    fn assign_object_property(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        source: Register,
        property: &AssignmentObjectProperty,
        taken: &mut Vec<Register>,
    ) -> Result<(), LowerError> {
        let key = self.property_key(builder, &property.name)?;
        taken.push(key);
        let element = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetProperty {
                dst: element,
                object: source,
                key,
            },
        )?;
        let element = match &property.initializer {
            Some(default) => self.apply_default(builder, range, element, default)?,
            None => element,
        };
        self.assign_target(builder, &property.target, element)
    }

    // ------------------------------------------------------------------
    // Functions and closures
    // ------------------------------------------------------------------

    fn lower_arrow(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        arrow: &ArrowFunction,
    ) -> Result<Register, LowerError> {
        let flags = FunctionFlags {
            is_async: arrow.is_async,
            is_generator: false,
        };
        let captures =
            self.compute_captures(&arrow.parameters, ArrowBody::Arrow(&arrow.body), true);
        let id = builder.reserve_function(range)?;
        self.build_function_into(
            builder,
            id,
            range,
            None,
            &arrow.parameters,
            ArrowBody::Arrow(&arrow.body),
            flags,
            &captures,
            true,
        )?;
        self.materialize_closure(builder, range, id, &captures)
    }

    fn build_constructible_function_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        name: Option<String>,
        function: &FunctionLike,
    ) -> Result<Register, LowerError> {
        let closure = self.build_function_value(builder, range, name, function)?;
        if !function.is_async && !function.is_generator {
            let prototype = self.alloc_register(range)?;
            self.emit(range, Instruction::CreateObject { dst: prototype })?;
            let constructor_key =
                self.string_reg(builder, EcmaString::from_utf8("constructor"), range)?;
            self.emit(
                range,
                Instruction::SetProperty {
                    object: prototype,
                    key: constructor_key,
                    value: closure,
                },
            )?;
            let prototype_key =
                self.string_reg(builder, EcmaString::from_utf8("prototype"), range)?;
            self.emit(
                range,
                Instruction::SetProperty {
                    object: closure,
                    key: prototype_key,
                    value: prototype,
                },
            )?;
        }
        Ok(closure)
    }

    /// Builds a function/method value (a closure) from a [`FunctionLike`].
    fn build_function_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        name: Option<String>,
        function: &FunctionLike,
    ) -> Result<Register, LowerError> {
        if let Some(decorator) = function.decorators.first() {
            return Err(self.unsupported(
                decorator.range(),
                UnsupportedConstruct::DecoratedDeclaration,
            ));
        }
        let flags = FunctionFlags {
            is_async: function.is_async,
            is_generator: function.is_generator,
        };
        let body = function
            .body
            .as_ref()
            .ok_or_else(|| self.missing(range, NodeKind::BlockStatement))?;
        let captures =
            self.compute_captures(&function.parameters, ArrowBody::Function(body), false);
        let id = builder.reserve_function(range)?;
        self.build_function_into(
            builder,
            id,
            range,
            name,
            &function.parameters,
            ArrowBody::Function(body),
            flags,
            &captures,
            false,
        )?;
        self.materialize_closure(builder, range, id, &captures)
    }

    /// Lowers a function body into a reserved module function slot.
    #[allow(clippy::too_many_arguments)]
    fn build_function_into(
        &mut self,
        builder: &mut ModuleBuilder,
        id: FunctionId,
        range: TextRange,
        name: Option<String>,
        parameters: &[ParameterNode],
        body: ArrowBody<'_>,
        flags: FunctionFlags,
        captures: &[CaptureKey],
        is_arrow: bool,
    ) -> Result<(), LowerError> {
        let capture_plan = CapturePlan::for_function(self.file, parameters, body);
        let mut inner = FunctionContext {
            file: self.file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            predeclared_cells: HashMap::new(),
            capture_plan,
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: false,
            goal: self.goal,
            completion: None,
            completion_pool: Vec::new(),
            completion_depth: 0,
            this_capture: None,
            new_target_capture: None,
            parent_constructor_capture: None,
            arguments_source: if is_arrow {
                ArgumentsSource::None
            } else {
                ArgumentsSource::Own
            },
        };
        // Leading capture registers.
        for capture in captures {
            let register = inner.alloc_register(range)?;
            inner.capture_count += 1;
            match capture {
                CaptureKey::Name(name) => {
                    inner.declare(
                        name.clone(),
                        Binding::Cell(register),
                        DeclarationScope::Function,
                    );
                }
                CaptureKey::This => inner.this_capture = Some(register),
                CaptureKey::Arguments => {
                    inner.arguments_source = ArgumentsSource::Captured(register);
                }
                CaptureKey::NewTarget => inner.new_target_capture = Some(register),
                CaptureKey::Parent(_) => inner.parent_constructor_capture = Some(register),
            }
        }
        // The function name binds to a self-closure register only when needed;
        // named function expressions refer to themselves via the environment
        // in this model, so no extra binding is required here.
        inner.bind_parameters(builder, parameters, range)?;
        if let ArrowBody::Function(FunctionBody::Block(block))
        | ArrowBody::Arrow(FunctionBody::Block(block)) = body
        {
            inner.hoist_vars(builder, &block.data().statements, range)?;
        }
        match body {
            ArrowBody::Function(FunctionBody::Block(block)) => {
                inner.lower_block(builder, block.data())?;
                inner.emit_return_undefined(builder, range)?;
            }
            ArrowBody::Arrow(FunctionBody::Block(block)) => {
                inner.lower_block(builder, block.data())?;
                inner.emit_return_undefined(builder, range)?;
            }
            ArrowBody::Arrow(FunctionBody::Expression(expression)) => {
                let value = inner.lower_expression(builder, expression)?;
                inner.emit(range, Instruction::Return { value })?;
            }
            ArrowBody::Function(FunctionBody::Expression(_)) => {
                return Err(self.missing(range, NodeKind::BlockStatement));
            }
            ArrowBody::Function(FunctionBody::Missing(missing))
            | ArrowBody::Arrow(FunctionBody::Missing(missing)) => {
                return Err(self.missing(range, missing.expected()));
            }
        }
        let name_constant = match name {
            Some(name) => {
                Some(builder.intern(Constant::String(EcmaString::from_utf8(&name)), range)?)
            }
            None => None,
        };
        let assembled = inner.into_function(name_constant, flags);
        builder.fill_function(id, assembled);
        Ok(())
    }

    /// Binds parameters into the leading (post-capture) registers, supporting
    /// simple, defaulted, destructured, and rest parameters.
    fn bind_parameters(
        &mut self,
        builder: &mut ModuleBuilder,
        parameters: &[ParameterNode],
        range: TextRange,
    ) -> Result<(), LowerError> {
        // Each non-rest parameter occupies one leading register slot.
        let rest_index = parameters.iter().position(|parameter| {
            matches!(parameter.data().binding.data(), BindingPattern::Rest(_))
        });
        let fixed = rest_index.unwrap_or(parameters.len());
        // Allocate one register per fixed parameter (the raw positional value).
        let mut slots = Vec::with_capacity(fixed);
        for _ in 0..fixed {
            let register = self.alloc_register(range)?;
            self.parameter_count += 1;
            slots.push(register);
        }
        // Captured parameter storage must exist before any default or
        // destructuring expression can materialize a closure over it.
        let mut undefined_seed = None;
        for (index, parameter) in parameters.iter().enumerate() {
            let mut names = Vec::new();
            collect_pattern_names(self.file, &parameter.data().binding, &mut names);
            for name in names {
                if !self.capture_plan.captures(
                    &name,
                    binding_site(parameter.range()),
                    DeclarationScope::Function,
                ) || self
                    .scopes
                    .first()
                    .is_some_and(|scope| scope.contains_key(&name))
                {
                    continue;
                }
                let seed = if index < fixed
                    && matches!(
                        parameter.data().binding.data(),
                        BindingPattern::Identifier(identifier)
                            if identifier_name(self.file, identifier).as_deref() == Some(&name)
                    ) {
                    slots[index]
                } else if let Some(seed) = undefined_seed {
                    seed
                } else {
                    let seed = self.undefined(builder, parameter.range())?;
                    undefined_seed = Some(seed);
                    seed
                };
                let cell = self.alloc_register(parameter.range())?;
                self.emit(parameter.range(), Instruction::CreateArray { dst: cell })?;
                self.emit(
                    parameter.range(),
                    Instruction::ArrayPush {
                        array: cell,
                        value: seed,
                    },
                )?;
                self.declare(name, Binding::Cell(cell), DeclarationScope::Function);
            }
        }
        for (index, parameter) in parameters.iter().take(fixed).enumerate() {
            let data = parameter.data();
            let slot = slots[index];
            let value = match &data.initializer {
                Some(default) => self.apply_default(builder, parameter.range(), slot, default)?,
                None => slot,
            };
            self.bind_pattern(builder, &data.binding, value, DeclarationScope::Function)?;
        }
        if let Some(rest_index) = rest_index {
            let parameter = &parameters[rest_index];
            let rest_argument = match parameter.data().binding.data() {
                BindingPattern::Rest(rest) => &rest.argument,
                _ => unreachable!("rest_index points at a rest binding"),
            };
            let rest = self.collect_rest_parameter(builder, range, fixed as u32)?;
            self.bind_pattern(builder, rest_argument, rest, DeclarationScope::Function)?;
        }
        Ok(())
    }

    /// Builds the rest-parameter array from `arguments[fixed..]`.
    fn collect_rest_parameter(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        fixed: u32,
    ) -> Result<Register, LowerError> {
        // Rest always receives the current activation's own actual arguments,
        // independent of the lexical `arguments` binding. For arrows the
        // `arguments` *identifier* stays captured from the enclosing function,
        // but `(...rest)` must observe this invocation's arguments, so load the
        // activation's arguments directly rather than routing through
        // `arguments_value`, which models the identifier's lexical semantics.
        let arguments = self.alloc_register(range)?;
        self.emit(range, Instruction::LoadArguments { dst: arguments })?;
        let iterator = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::GetIterator {
                dst: iterator,
                src: arguments,
                kind: IteratorKind::Sync,
            },
        )?;
        for _ in 0..fixed {
            self.iterator_step_discard(range, iterator)?;
        }
        self.rest_array(builder, range, iterator)
    }

    fn emit_return_undefined(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
    ) -> Result<(), LowerError> {
        let value = self.undefined(builder, range)?;
        self.emit(range, Instruction::Return { value })?;
        Ok(())
    }

    /// Builds the captures array in the enclosing function and creates the
    /// closure over `id`.
    fn materialize_closure(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        id: FunctionId,
        captures: &[CaptureKey],
    ) -> Result<Register, LowerError> {
        if captures.len() > MAX_REGISTERS as usize {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Captures)));
        }
        let array = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateArray { dst: array })?;
        for capture in captures {
            let value = self.capture_value(builder, range, capture)?;
            self.emit(range, Instruction::ArrayPush { array, value })?;
        }
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::CreateClosure {
                dst,
                function: id,
                captures: array,
            },
        )?;
        Ok(dst)
    }

    fn capture_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        capture: &CaptureKey,
    ) -> Result<Register, LowerError> {
        match capture {
            CaptureKey::Name(name) => match self.resolve(name) {
                Some(Binding::Cell(cell)) => Ok(cell),
                Some(Binding::Local(_)) => {
                    panic!("capture plan resolved named capture `{name}` to Local")
                }
                None => Err(self.error(
                    range,
                    LowerErrorKind::MissingSyntax {
                        expected: NodeKind::Identifier,
                    },
                )),
            },
            CaptureKey::This => self.this_value(range),
            CaptureKey::Arguments => match self.arguments_value(builder, range)? {
                Some(register) => Ok(register),
                None => self.undefined(builder, range),
            },
            CaptureKey::NewTarget => self.new_target_value(range),
            CaptureKey::Parent(parent) => Ok(*parent),
        }
    }

    /// Computes the deterministic capture list of a nested function: the free
    /// variable names it reads that resolve to a function-local binding of this
    /// (enclosing) context, plus `this`/`arguments`/`new.target` for arrows.
    fn compute_captures(
        &self,
        parameters: &[ParameterNode],
        body: ArrowBody<'_>,
        is_arrow: bool,
    ) -> Vec<CaptureKey> {
        let mut scanner = FreeVarScanner::new(self.file);
        scanner.scan_function(parameters, body, is_arrow);
        let mut captures = Vec::new();
        for name in &scanner.free {
            if self.resolve(name).is_some() {
                captures.push(CaptureKey::Name(name.clone()));
            }
        }
        if is_arrow {
            if scanner.uses_this {
                captures.push(CaptureKey::This);
            }
            if scanner.uses_arguments {
                captures.push(CaptureKey::Arguments);
            }
            if scanner.uses_new_target {
                captures.push(CaptureKey::NewTarget);
            }
        }
        captures
    }

    // ------------------------------------------------------------------
    // Classes
    // ------------------------------------------------------------------

    fn lower_class_declaration(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        class: &ClassDeclaration,
        forced_name: Option<&str>,
    ) -> Result<(), LowerError> {
        let name = match forced_name {
            Some(name) => Some(name.to_owned()),
            None => match &class.name {
                Some(identifier) => Some(self.identifier_text(identifier)?),
                None => None,
            },
        };
        self.lower_class_value(builder, range, class, name.as_deref(), None)?;
        Ok(())
    }

    /// Builds a class as a constructor function plus a prototype object with
    /// methods, accessors, static members, private names, and prototype chain.
    fn lower_class_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        class: &ClassDeclaration,
        declaration_name: Option<&str>,
        expression_name: Option<(&str, BindingSite)>,
    ) -> Result<Register, LowerError> {
        if let Some(decorator) = class.decorators.first() {
            return Err(self.unsupported(
                decorator.range(),
                UnsupportedConstruct::DecoratedDeclaration,
            ));
        }
        let expression_cell = if let Some((name, site)) = expression_name {
            self.push_scope();
            Some(self.predeclare_class_expression_binding(name, range, site)?)
        } else {
            None
        };
        // Class declarations retain their predeclared outer cell. A named class
        // expression instead shadows it with its own uninitialized cell here.
        let parent = match &class.extends {
            Some(heritage) => Some(self.lower_expression(builder, &heritage.expression)?),
            None => None,
        };
        let declaration_target = if let Some(name) = declaration_name {
            let site = class
                .name
                .as_ref()
                .map_or(binding_site(range), |identifier| {
                    binding_site(identifier.range())
                });
            self.predeclare_captured_binding(name, range, site, DeclarationScope::Lexical)?;
            let identity = binding_identity(name, site, DeclarationScope::Lexical);
            if self.top_level {
                None
            } else if let Some(cell) = self.predeclared_cells.get(&identity).copied() {
                Some(Binding::Cell(cell))
            } else {
                let home = self.alloc_register(range)?;
                self.declare(
                    name.to_owned(),
                    Binding::Local(home),
                    DeclarationScope::Lexical,
                );
                Some(Binding::Local(home))
            }
        } else {
            None
        };
        if expression_cell.is_none() {
            self.push_scope();
        }
        self.create_private_names(builder, range, class)?;
        let constructor = self.find_constructor(class);
        let ctor = self.build_constructor(builder, range, class, constructor, parent)?;
        let prototype = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateObject { dst: prototype })?;
        if let Some(parent) = parent {
            let parent_prototype = self.get_named(builder, range, parent, "prototype")?;
            self.emit(
                range,
                Instruction::SetPrototype {
                    object: prototype,
                    prototype: parent_prototype,
                },
            )?;
            self.emit(
                range,
                Instruction::SetPrototype {
                    object: ctor,
                    prototype: parent,
                },
            )?;
        }
        let prototype_key = self.string_reg(builder, EcmaString::from_utf8("prototype"), range)?;
        self.emit(
            range,
            Instruction::SetProperty {
                object: ctor,
                key: prototype_key,
                value: prototype,
            },
        )?;
        if let Some(cell) = expression_cell {
            self.store_cell(builder, cell, ctor, range)?;
        }
        if let Some(name) = declaration_name {
            match declaration_target {
                Some(Binding::Local(home)) => self.move_to(range, home, ctor)?,
                Some(Binding::Cell(cell)) => self.store_cell(builder, cell, ctor, range)?,
                None => {
                    debug_assert!(self.top_level);
                    let id =
                        builder.intern(Constant::String(EcmaString::from_utf8(name)), range)?;
                    self.emit(
                        range,
                        Instruction::StoreGlobal {
                            name: id,
                            value: ctor,
                        },
                    )?;
                }
            }
        }
        for member in &class.members {
            self.lower_class_member(builder, ctor, prototype, member)?;
        }
        self.pop_scope();
        Ok(ctor)
    }

    fn find_constructor<'c>(
        &self,
        class: &'c ClassDeclaration,
    ) -> Option<&'c crate::syntax::ConstructorDeclaration> {
        class.members.iter().find_map(|member| match member.data() {
            ClassMember::Constructor(constructor) => Some(constructor),
            _ => None,
        })
    }

    /// Creates the class's declared private names as register-local bindings so
    /// method and constructor bodies capture and address them uniformly.
    fn create_private_names(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        class: &ClassDeclaration,
    ) -> Result<(), LowerError> {
        let mut seen = HashSet::new();
        for member in &class.members {
            let name = match member.data() {
                ClassMember::Method(method) => &method.name,
                ClassMember::Property(property) => &property.name,
                ClassMember::AutoAccessor(accessor) => &accessor.name,
                _ => continue,
            };
            if let PropertyName::Private(private) = name {
                let text = self.private_text(private)?;
                if !seen.insert(text.clone()) {
                    continue;
                }
                let description =
                    builder.intern(Constant::String(EcmaString::from_utf8(&text)), range)?;
                let value = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::CreatePrivateName {
                        dst: value,
                        description,
                    },
                )?;
                if self.capture_plan.captures(
                    &text,
                    binding_site(private.range()),
                    DeclarationScope::Lexical,
                ) {
                    let cell = self.alloc_register(range)?;
                    self.emit(range, Instruction::CreateArray { dst: cell })?;
                    self.emit(range, Instruction::ArrayPush { array: cell, value })?;
                    self.declare(text, Binding::Cell(cell), DeclarationScope::Lexical);
                } else {
                    self.declare(text, Binding::Local(value), DeclarationScope::Lexical);
                }
            }
        }
        Ok(())
    }

    fn build_constructor(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        class: &ClassDeclaration,
        constructor: Option<&crate::syntax::ConstructorDeclaration>,
        parent: Option<Register>,
    ) -> Result<Register, LowerError> {
        // Instance field initializers to run in the constructor.
        let fields: Vec<&crate::syntax::ClassProperty> = class
            .members
            .iter()
            .filter_map(|member| match member.data() {
                ClassMember::Property(property)
                    if !property.modifiers.is_static
                        && !property.modifiers.is_abstract
                        && !property.modifiers.is_declare =>
                {
                    Some(property)
                }
                _ => None,
            })
            .collect();
        let (parameters, body_block): (&[ParameterNode], Option<&Block>) = match constructor {
            Some(constructor) => (&constructor.parameters, Some(constructor.body.data())),
            None => (&[], None),
        };
        let captures = self.compute_constructor_captures(parameters, body_block, &fields, parent);
        let id = builder.reserve_function(range)?;
        self.build_constructor_into(
            builder,
            id,
            range,
            parameters,
            body_block,
            &fields,
            &captures,
            parent.is_some(),
        )?;
        self.materialize_closure(builder, range, id, &captures)
    }

    fn compute_constructor_captures(
        &self,
        parameters: &[ParameterNode],
        body: Option<&Block>,
        fields: &[&crate::syntax::ClassProperty],
        parent: Option<Register>,
    ) -> Vec<CaptureKey> {
        let mut scanner = FreeVarScanner::new(self.file);
        scanner.preseed_parameters(parameters);
        if let Some(block) = body {
            scanner.preseed_vars(&block.statements);
        }
        scanner.scan_parameter_initializers(parameters);
        if let Some(block) = body {
            for statement in &block.statements {
                scanner.scan_statement(statement);
            }
        }
        for field in fields {
            scanner.scan_property_name(&field.name);
            if let Some(initializer) = &field.initializer {
                scanner.scan_expression(initializer);
            }
        }
        let mut captures = Vec::new();
        for name in &scanner.free {
            if self.resolve(name).is_some() {
                captures.push(CaptureKey::Name(name.clone()));
            }
        }
        if let Some(parent) = parent {
            captures.push(CaptureKey::Parent(parent));
        }
        captures
    }

    fn derived_super_index(&self, body: &Block) -> Result<usize, LowerError> {
        let mut direct = None;
        for (index, statement) in body.statements.iter().enumerate() {
            if let Statement::Expression(expression) = statement.data()
                && let Expression::Call(call) = expression.expression.data()
                && !call.optional
                && matches!(call.callee.data(), Expression::Super)
                && direct.replace(index).is_some()
            {
                return Err(self.unsupported(
                    statement.range(),
                    UnsupportedConstruct::DerivedConstructorShape,
                ));
            }
        }
        let Some(index) = direct else {
            return Err(self.unsupported(
                body.statements
                    .first()
                    .map_or_else(zero_range, |statement| statement.range()),
                UnsupportedConstruct::DerivedConstructorShape,
            ));
        };
        let first = body
            .statements
            .first()
            .expect("direct super requires a statement");
        let last = body
            .statements
            .last()
            .expect("direct super requires a statement");
        let super_count = self
            .file
            .tokens()
            .iter()
            .filter(|token| {
                token.kind() == TokenKind::KwSuper
                    && token.range().start() >= first.range().start()
                    && token.range().end() <= last.range().end()
            })
            .count();
        if super_count != 1 {
            return Err(self.unsupported(
                body.statements[index].range(),
                UnsupportedConstruct::DerivedConstructorShape,
            ));
        }
        let super_statement = body.statements[index].range();
        if self.file.tokens().iter().any(|token| {
            token.kind() == TokenKind::KwThis
                && token.range().start() >= first.range().start()
                && token.range().end() <= super_statement.end()
        }) {
            return Err(self.unsupported(
                super_statement,
                UnsupportedConstruct::ThisBeforeDerivedSuper,
            ));
        }
        Ok(index)
    }

    fn initialize_instance_fields(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        fields: &[&crate::syntax::ClassProperty],
    ) -> Result<(), LowerError> {
        for field in fields {
            let this_value = self.this_value(range)?;
            let key = self.property_key(builder, &field.name)?;
            let value = match &field.initializer {
                Some(initializer) => self.lower_expression(builder, initializer)?,
                None => self.undefined(builder, range)?,
            };
            self.emit(
                range,
                Instruction::SetProperty {
                    object: this_value,
                    key,
                    value,
                },
            )?;
        }
        Ok(())
    }

    fn lower_derived_super(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        call: &CallExpression,
    ) -> Result<(), LowerError> {
        let parent = self.parent_constructor_capture.ok_or_else(|| {
            self.unsupported(range, UnsupportedConstruct::DerivedConstructorShape)
        })?;
        let this_value = self.this_value(range)?;
        let arguments = self.build_arguments(builder, range, &call.arguments)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst,
                callee: parent,
                this_value,
                arguments,
            },
        )?;
        Ok(())
    }

    fn lower_implicit_derived_super(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
    ) -> Result<(), LowerError> {
        let parent = self.parent_constructor_capture.ok_or_else(|| {
            self.unsupported(range, UnsupportedConstruct::DerivedConstructorShape)
        })?;
        let this_value = self.this_value(range)?;
        let arguments = self.arguments_value(builder, range)?.ok_or_else(|| {
            self.unsupported(range, UnsupportedConstruct::DerivedConstructorShape)
        })?;
        let call_arguments = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::CreateArray {
                dst: call_arguments,
            },
        )?;
        self.emit(
            range,
            Instruction::ArrayExtend {
                array: call_arguments,
                iterable: arguments,
            },
        )?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst,
                callee: parent,
                this_value,
                arguments: call_arguments,
            },
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_constructor_into(
        &mut self,
        builder: &mut ModuleBuilder,
        id: FunctionId,
        range: TextRange,
        parameters: &[ParameterNode],
        body: Option<&Block>,
        fields: &[&crate::syntax::ClassProperty],
        captures: &[CaptureKey],
        derived: bool,
    ) -> Result<(), LowerError> {
        let capture_plan = CapturePlan::for_constructor(self.file, parameters, body, fields);
        let mut inner = FunctionContext {
            file: self.file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            predeclared_cells: HashMap::new(),
            capture_plan,
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: false,
            goal: self.goal,
            completion: None,
            completion_pool: Vec::new(),
            completion_depth: 0,
            this_capture: None,
            new_target_capture: None,
            parent_constructor_capture: None,
            arguments_source: ArgumentsSource::Own,
        };
        for capture in captures {
            let register = inner.alloc_register(range)?;
            inner.capture_count += 1;
            match capture {
                CaptureKey::Name(name) => {
                    inner.declare(
                        name.clone(),
                        Binding::Cell(register),
                        DeclarationScope::Function,
                    );
                }
                CaptureKey::Parent(_) => inner.parent_constructor_capture = Some(register),
                CaptureKey::This | CaptureKey::Arguments | CaptureKey::NewTarget => {
                    unreachable!("constructors do not capture arrow-only bindings")
                }
            }
        }
        inner.bind_parameters(builder, parameters, range)?;
        if let Some(block) = body {
            inner.hoist_vars(builder, &block.statements, range)?;
        }
        if derived {
            if let Some(block) = body {
                let super_index = inner.derived_super_index(block)?;
                inner.push_scope();
                for statement in &block.statements[..super_index] {
                    inner.lower_statement(builder, statement)?;
                }
                let Statement::Expression(expression) = block.statements[super_index].data() else {
                    unreachable!("derived_super_index selects an expression statement");
                };
                let Expression::Call(call) = expression.expression.data() else {
                    unreachable!("derived_super_index selects a call expression");
                };
                inner.lower_derived_super(builder, block.statements[super_index].range(), call)?;
                let body_scope = inner.scopes.pop().expect("constructor block scope exists");
                inner.initialize_instance_fields(builder, range, fields)?;
                inner.scopes.push(body_scope);
                for statement in &block.statements[super_index + 1..] {
                    inner.lower_statement(builder, statement)?;
                }
                inner.pop_scope();
            } else {
                inner.lower_implicit_derived_super(builder, range)?;
                inner.initialize_instance_fields(builder, range, fields)?;
            }
        } else {
            inner.initialize_instance_fields(builder, range, fields)?;
            if let Some(block) = body {
                inner.lower_block(builder, block)?;
            }
        }
        inner.emit_return_undefined(builder, range)?;
        let assembled = inner.into_function(None, FunctionFlags::default());
        builder.fill_function(id, assembled);
        Ok(())
    }

    fn lower_class_member(
        &mut self,
        builder: &mut ModuleBuilder,
        ctor: Register,
        prototype: Register,
        member: &crate::syntax::ClassMemberNode,
    ) -> Result<(), LowerError> {
        let range = member.range();
        match member.data() {
            ClassMember::Constructor(_) => Ok(()),
            ClassMember::Method(method) => {
                if method.function.body.is_none() {
                    // Abstract method or overload signature: type-only.
                    return Ok(());
                }
                let target = if method.modifiers.is_static {
                    ctor
                } else {
                    prototype
                };
                let key = self.property_key(builder, &method.name)?;
                let value = self.build_function_value(builder, range, None, &method.function)?;
                self.install_property(builder, range, target, key, value, method.modifier)
            }
            ClassMember::Property(property) => {
                if property.modifiers.is_abstract || property.modifiers.is_declare {
                    // Type-only field declaration: no runtime slot.
                    return Ok(());
                }
                if property.modifiers.is_static {
                    // Static field: ctor[key] = init.
                    let key = self.property_key(builder, &property.name)?;
                    let value = match &property.initializer {
                        Some(initializer) => self.lower_expression(builder, initializer)?,
                        None => self.undefined(builder, range)?,
                    };
                    self.emit(
                        range,
                        Instruction::SetProperty {
                            object: ctor,
                            key,
                            value,
                        },
                    )?;
                }
                // Instance fields are initialized in the constructor.
                Ok(())
            }
            ClassMember::AutoAccessor(accessor) => {
                if accessor.modifiers.is_abstract || accessor.modifiers.is_declare {
                    return Ok(());
                }
                // An auto-accessor is modeled as a plain data property on its
                // target for structural purposes.
                let target = if accessor.modifiers.is_static {
                    ctor
                } else {
                    prototype
                };
                let key = self.property_key(builder, &accessor.name)?;
                let value = match &accessor.initializer {
                    Some(initializer) => self.lower_expression(builder, initializer)?,
                    None => self.undefined(builder, range)?,
                };
                self.emit(
                    range,
                    Instruction::SetProperty {
                        object: target,
                        key,
                        value,
                    },
                )?;
                Ok(())
            }
            ClassMember::StaticBlock(block) => {
                // A static initialization block runs at class definition time
                // in the enclosing scope, with `this` bound to the constructor.
                self.push_scope();
                let result = self.lower_block(builder, block.data());
                self.pop_scope();
                result
            }
            ClassMember::IndexSignature(_) => Ok(()),
            ClassMember::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }
}

/// A function body that is either a classic function body or an arrow body.
#[derive(Clone, Copy)]
enum ArrowBody<'a> {
    Function(&'a FunctionBody),
    Arrow(&'a FunctionBody),
}

#[derive(Clone)]
struct ScannedBinding {
    identity: BindingIdentity,
    owner_depth: u32,
}

/// Collects free variables, special lexical uses, and captured root binding
/// identities using the same declaration timeline as lowering.
struct FreeVarScanner<'a> {
    file: &'a SourceFile,
    bound: Vec<HashMap<String, ScannedBinding>>,
    function_roots: Vec<usize>,
    free: BTreeSet<String>,
    captured: HashSet<BindingIdentity>,
    runtime_cells: HashSet<BindingIdentity>,
    initialized: HashSet<BindingIdentity>,
    uses_this: bool,
    uses_arguments: bool,
    uses_new_target: bool,
    fn_boundary: u32,
    function_depth: u32,
}

impl CapturePlan {
    fn for_statements(file: &SourceFile, statements: &[Stmt]) -> Self {
        let mut scanner = FreeVarScanner::new(file);
        scanner.preseed_vars(statements);
        scanner.predeclare_immediate(statements, false);
        for statement in statements {
            scanner.scan_statement(statement);
        }
        Self {
            captured: scanner.captured,
            runtime_cells: scanner.runtime_cells,
        }
    }

    fn for_function(file: &SourceFile, parameters: &[ParameterNode], body: ArrowBody<'_>) -> Self {
        let mut scanner = FreeVarScanner::new(file);
        scanner.scan_function(parameters, body, false);
        Self {
            captured: scanner.captured,
            runtime_cells: scanner.runtime_cells,
        }
    }

    fn for_constructor(
        file: &SourceFile,
        parameters: &[ParameterNode],
        body: Option<&Block>,
        fields: &[&crate::syntax::ClassProperty],
    ) -> Self {
        let mut scanner = FreeVarScanner::new(file);
        scanner.preseed_parameters(parameters);
        if let Some(block) = body {
            scanner.preseed_vars(&block.statements);
            scanner.predeclare_immediate(&block.statements, false);
        }
        scanner.scan_parameter_initializers(parameters);
        if let Some(block) = body {
            for statement in &block.statements {
                scanner.scan_statement(statement);
            }
        }
        for field in fields {
            scanner.scan_property_name(&field.name);
            if let Some(initializer) = &field.initializer {
                scanner.scan_expression(initializer);
            }
        }
        Self {
            captured: scanner.captured,
            runtime_cells: scanner.runtime_cells,
        }
    }
}

impl<'a> FreeVarScanner<'a> {
    fn new(file: &'a SourceFile) -> Self {
        Self {
            file,
            bound: vec![HashMap::new()],
            function_roots: vec![0],
            free: BTreeSet::new(),
            captured: HashSet::new(),
            runtime_cells: HashSet::new(),
            initialized: HashSet::new(),
            uses_this: false,
            uses_arguments: false,
            uses_new_target: false,
            fn_boundary: 0,
            function_depth: 0,
        }
    }

    fn scan_function(
        &mut self,
        parameters: &[ParameterNode],
        body: ArrowBody<'_>,
        _is_arrow: bool,
    ) {
        self.preseed_parameters(parameters);
        if let ArrowBody::Function(FunctionBody::Block(block))
        | ArrowBody::Arrow(FunctionBody::Block(block)) = body
        {
            self.preseed_vars(&block.data().statements);
            self.predeclare_immediate(&block.data().statements, false);
        }
        self.scan_parameter_initializers(parameters);
        match body {
            ArrowBody::Function(FunctionBody::Block(block))
            | ArrowBody::Arrow(FunctionBody::Block(block)) => {
                for statement in &block.data().statements {
                    self.scan_statement(statement);
                }
            }
            ArrowBody::Arrow(FunctionBody::Expression(expression))
            | ArrowBody::Function(FunctionBody::Expression(expression)) => {
                self.scan_expression(expression);
            }
            _ => {}
        }
    }

    fn push(&mut self) {
        self.bound.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.bound.pop();
    }

    fn bind_function(&mut self, name: String) {
        let root = *self
            .function_roots
            .last()
            .expect("scanner always has a function root");
        self.bound[root]
            .entry(name.clone())
            .or_insert(ScannedBinding {
                identity: BindingIdentity::Function(name),
                owner_depth: self.function_depth,
            });
    }

    fn bind_lexical(&mut self, name: String, range: TextRange) {
        if let Some(scope) = self.bound.last_mut() {
            scope.insert(
                name,
                ScannedBinding {
                    identity: BindingIdentity::Lexical(binding_site(range)),
                    owner_depth: self.function_depth,
                },
            );
        }
    }

    fn resolve_binding(&self, name: &str) -> Option<&ScannedBinding> {
        self.bound.iter().rev().find_map(|scope| scope.get(name))
    }

    fn scan_property_name(&mut self, name: &PropertyName) {
        match name {
            PropertyName::Computed(expression) => self.scan_expression(expression),
            PropertyName::Private(private) => {
                if let Some(text) = private_name(self.file, private) {
                    self.use_name(&text);
                }
            }
            _ => {}
        }
    }

    fn use_name(&mut self, name: &str) {
        if name == "arguments" {
            if self.fn_boundary == 0 {
                self.uses_arguments = true;
            }
            return;
        }
        if let Some(binding) = self.resolve_binding(name).cloned() {
            if binding.owner_depth == 0 && !self.initialized.contains(&binding.identity) {
                self.runtime_cells.insert(binding.identity.clone());
            }
            if self.function_depth > binding.owner_depth && binding.owner_depth == 0 {
                self.captured.insert(binding.identity);
            }
        } else {
            self.free.insert(name.to_owned());
        }
    }

    fn predeclare_immediate(&mut self, statements: &[Stmt], switch_scope: bool) {
        for declaration in collect_immediate_declarations(self.file, statements) {
            match declaration.kind {
                ImmediateDeclarationKind::Function(_) => {
                    let identity = BindingIdentity::Function(declaration.name.clone());
                    self.bind_function(declaration.name);
                    self.initialized.insert(identity);
                }
                ImmediateDeclarationKind::Lexical => {
                    let identity = BindingIdentity::Lexical(declaration.site);
                    self.bind_lexical(declaration.name, declaration.range);
                    if switch_scope {
                        self.runtime_cells.insert(identity);
                    }
                }
            }
        }
    }

    fn initialize_pattern(&mut self, pattern: &Pattern, declaration_scope: DeclarationScope) {
        let mut names = Vec::new();
        collect_pattern_names(self.file, pattern, &mut names);
        for name in names {
            if let Some(binding) = self.resolve_binding(&name)
                && (matches!(declaration_scope, DeclarationScope::Function)
                    || binding.owner_depth == self.function_depth)
            {
                self.initialized.insert(binding.identity.clone());
            }
        }
    }

    fn preseed_parameters(&mut self, parameters: &[ParameterNode]) {
        for parameter in parameters {
            let mut names = Vec::new();
            collect_pattern_names(self.file, &parameter.data().binding, &mut names);
            for name in names {
                self.bind_function(name.clone());
                self.initialized.insert(BindingIdentity::Function(name));
            }
        }
    }

    fn preseed_vars(&mut self, statements: &[Stmt]) {
        let mut names = Vec::new();
        collect_var_names(self.file, statements, &mut names);
        for name in names {
            self.bind_function(name.clone());
            self.initialized.insert(BindingIdentity::Function(name));
        }
    }

    fn scan_parameter_initializers(&mut self, parameters: &[ParameterNode]) {
        for parameter in parameters {
            let data = parameter.data();
            if let Some(initializer) = &data.initializer {
                self.scan_expression(initializer);
            }
            self.scan_pattern_effects(&data.binding);
        }
    }

    fn scan_pattern_effects(&mut self, pattern: &Pattern) {
        match pattern.data() {
            BindingPattern::Identifier(_) | BindingPattern::Missing(_) => {}
            BindingPattern::Object(object) => {
                for property in &object.properties {
                    if let PropertyName::Computed(expression) = &property.name {
                        self.scan_expression(expression);
                    }
                    if let Some(initializer) = &property.initializer {
                        self.scan_expression(initializer);
                    }
                    self.scan_pattern_effects(&property.binding);
                }
            }
            BindingPattern::Array(array) => {
                for element in &array.elements {
                    if let ArrayBindingElement::Binding(inner) = element {
                        self.scan_pattern_effects(inner);
                    }
                }
            }
            BindingPattern::Rest(rest) => self.scan_pattern_effects(&rest.argument),
            BindingPattern::Assignment(assignment) => {
                self.scan_expression(&assignment.right);
                self.scan_pattern_effects(&assignment.left);
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, declaration_scope: DeclarationScope) {
        match pattern.data() {
            BindingPattern::Identifier(identifier) => {
                if let Some(text) = identifier_name(self.file, identifier) {
                    match declaration_scope {
                        DeclarationScope::Function => self.bind_function(text),
                        DeclarationScope::Lexical | DeclarationScope::Iteration => {
                            self.bind_lexical(text, identifier.range());
                        }
                    }
                }
            }
            BindingPattern::Object(object) => {
                for property in &object.properties {
                    self.bind_pattern(&property.binding, declaration_scope);
                }
            }
            BindingPattern::Array(array) => {
                for element in &array.elements {
                    if let ArrayBindingElement::Binding(inner) = element {
                        self.bind_pattern(inner, declaration_scope);
                    }
                }
            }
            BindingPattern::Rest(rest) => self.bind_pattern(&rest.argument, declaration_scope),
            BindingPattern::Assignment(assignment) => {
                self.bind_pattern(&assignment.left, declaration_scope);
            }
            BindingPattern::Missing(_) => {}
        }
    }

    fn scan_statement(&mut self, statement: &Stmt) {
        match statement.data() {
            Statement::Variable(declaration) => {
                let scope = match declaration.kind {
                    VariableKind::Var => DeclarationScope::Function,
                    _ => DeclarationScope::Lexical,
                };
                for declarator in &declaration.declarations {
                    if let Some(initializer) = &declarator.data().initializer {
                        self.scan_expression(initializer);
                    }
                    self.scan_pattern_effects(&declarator.data().binding);
                    self.initialize_pattern(&declarator.data().binding, scope);
                }
            }
            Statement::Function(declaration) => {
                self.scan_function_like(&declaration.function);
            }
            Statement::Class(class) => {
                self.scan_class_heritage(class);
                if let Some(name) = &class.name
                    && let Some(text) = identifier_name(self.file, name)
                    && let Some(binding) = self.resolve_binding(&text)
                {
                    self.initialized.insert(binding.identity.clone());
                }
                self.scan_class(class);
            }
            Statement::Expression(expression) => self.scan_expression(&expression.expression),
            Statement::Return(statement) => {
                if let Some(argument) = &statement.argument {
                    self.scan_expression(argument);
                }
            }
            Statement::Throw(statement) => self.scan_expression(&statement.argument),
            Statement::If(statement) => {
                self.scan_expression(&statement.test);
                self.scan_statement(&statement.consequent);
                if let Some(alternate) = &statement.alternate {
                    self.scan_statement(alternate);
                }
            }
            Statement::Block(block) => {
                self.push();
                self.predeclare_immediate(&block.data().statements, false);
                for statement in &block.data().statements {
                    self.scan_statement(statement);
                }
                self.pop();
            }
            Statement::While(statement) => {
                self.scan_expression(&statement.test);
                self.scan_statement(&statement.body);
            }
            Statement::DoWhile(statement) => {
                self.scan_statement(&statement.body);
                self.scan_expression(&statement.test);
            }
            Statement::For(statement) => {
                self.push();
                if let Some(initializer) = &statement.initializer {
                    match initializer {
                        ForInitializer::Variable(declaration) => {
                            let scope = match declaration.kind {
                                VariableKind::Var => DeclarationScope::Function,
                                _ => DeclarationScope::Lexical,
                            };
                            for declarator in &declaration.declarations {
                                self.bind_pattern(&declarator.data().binding, scope);
                                if let Some(init) = &declarator.data().initializer {
                                    self.scan_expression(init);
                                }
                                self.scan_pattern_effects(&declarator.data().binding);
                                self.initialize_pattern(&declarator.data().binding, scope);
                            }
                        }
                        ForInitializer::Expression(expression) => self.scan_expression(expression),
                    }
                }
                if let Some(test) = &statement.test {
                    self.scan_expression(test);
                }
                if let Some(update) = &statement.update {
                    self.scan_expression(update);
                }
                self.scan_statement(&statement.body);
                self.pop();
            }
            Statement::ForIn(statement) => {
                self.push();
                self.scan_expression(&statement.object);
                self.scan_for_binding(&statement.binding);
                self.scan_statement(&statement.body);
                self.pop();
            }
            Statement::ForOf(statement) => {
                self.push();
                self.scan_expression(&statement.iterable);
                self.scan_for_binding(&statement.binding);
                self.scan_statement(&statement.body);
                self.pop();
            }
            Statement::Switch(statement) => {
                self.scan_expression(&statement.discriminant);
                self.push();
                let statements = statement
                    .cases
                    .iter()
                    .flat_map(|case| case.data().consequent.iter().cloned())
                    .collect::<Vec<_>>();
                self.predeclare_immediate(&statements, true);
                for case in &statement.cases {
                    if let Some(test) = &case.data().test {
                        self.scan_expression(test);
                    }
                    for statement in &case.data().consequent {
                        self.scan_statement(statement);
                    }
                }
                self.pop();
            }
            Statement::Try(statement) => {
                self.push();
                self.predeclare_immediate(&statement.block.data().statements, false);
                for statement in &statement.block.data().statements {
                    self.scan_statement(statement);
                }
                self.pop();
                if let Some(handler) = &statement.handler {
                    self.push();
                    if let Some(binding) = &handler.data().binding {
                        self.bind_pattern(binding, DeclarationScope::Lexical);
                        self.initialize_pattern(binding, DeclarationScope::Lexical);
                    }
                    self.predeclare_immediate(&handler.data().body.data().statements, false);
                    for statement in &handler.data().body.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                }
                if let Some(finalizer) = &statement.finalizer {
                    self.push();
                    self.predeclare_immediate(&finalizer.data().statements, false);
                    for statement in &finalizer.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                }
            }
            Statement::Labeled(statement) => self.scan_statement(&statement.body),
            Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
                statement,
            ))) => self.scan_statement(statement),
            Statement::Export(ExportDeclaration::Default(default)) => match &default.value {
                ExportDefaultValue::Expression(expression) => self.scan_expression(expression),
                ExportDefaultValue::Function(function) => {
                    if function.body.is_some()
                        && let Some(name) = &function.name
                        && let Some(text) = identifier_name(self.file, name)
                    {
                        self.bind_function(text);
                    }
                    self.scan_function_like(function);
                }
                ExportDefaultValue::Class(class) => {
                    self.scan_class_heritage(class);
                    if let Some(name) = &class.name
                        && let Some(text) = identifier_name(self.file, name)
                    {
                        self.bind_lexical(text, name.range());
                    }
                    self.scan_class(class);
                }
                ExportDefaultValue::Missing(_) => {}
            },
            _ => {}
        }
    }

    fn scan_for_binding(&mut self, binding: &ForBinding) {
        match binding {
            ForBinding::Variable(declaration) => {
                let scope = match declaration.kind {
                    VariableKind::Var => DeclarationScope::Function,
                    _ => DeclarationScope::Iteration,
                };
                for declarator in &declaration.declarations {
                    self.bind_pattern(&declarator.data().binding, scope);
                    self.scan_pattern_effects(&declarator.data().binding);
                }
            }
            ForBinding::Target(target) => self.scan_assignment_target(target),
        }
    }

    fn scan_function_like(&mut self, function: &FunctionLike) {
        self.fn_boundary += 1;
        self.function_depth += 1;
        self.push();
        self.function_roots.push(self.bound.len() - 1);
        self.preseed_parameters(&function.parameters);
        if let Some(FunctionBody::Block(block)) = &function.body {
            self.preseed_vars(&block.data().statements);
            self.predeclare_immediate(&block.data().statements, false);
        }
        self.scan_parameter_initializers(&function.parameters);
        if let Some(body) = &function.body {
            match body {
                FunctionBody::Block(block) => {
                    for statement in &block.data().statements {
                        self.scan_statement(statement);
                    }
                }
                FunctionBody::Expression(expression) => self.scan_expression(expression),
                FunctionBody::Missing(_) => {}
            }
        }
        self.function_roots.pop();
        self.pop();
        self.function_depth -= 1;
        self.fn_boundary -= 1;
    }

    fn scan_arrow(&mut self, arrow: &ArrowFunction) {
        self.function_depth += 1;
        self.push();
        self.function_roots.push(self.bound.len() - 1);
        self.preseed_parameters(&arrow.parameters);
        if let FunctionBody::Block(block) = &arrow.body {
            self.preseed_vars(&block.data().statements);
            self.predeclare_immediate(&block.data().statements, false);
        }
        self.scan_parameter_initializers(&arrow.parameters);
        match &arrow.body {
            FunctionBody::Block(block) => {
                for statement in &block.data().statements {
                    self.scan_statement(statement);
                }
            }
            FunctionBody::Expression(expression) => self.scan_expression(expression),
            FunctionBody::Missing(_) => {}
        }
        self.function_roots.pop();
        self.pop();
        self.function_depth -= 1;
    }

    fn scan_class_heritage(&mut self, class: &ClassDeclaration) {
        if let Some(heritage) = &class.extends {
            self.scan_expression(&heritage.expression);
        }
    }

    fn scan_class(&mut self, class: &ClassDeclaration) {
        self.push();
        let mut seen_private = HashSet::new();
        for member in &class.members {
            let name = match member.data() {
                ClassMember::Method(method) => &method.name,
                ClassMember::Property(property) => &property.name,
                ClassMember::AutoAccessor(accessor) => &accessor.name,
                _ => continue,
            };
            if let PropertyName::Private(private) = name
                && let Some(text) = private_name(self.file, private)
                && seen_private.insert(text.clone())
            {
                self.bind_lexical(text, private.range());
            }
        }
        let constructor = class.members.iter().find_map(|member| match member.data() {
            ClassMember::Constructor(constructor) => Some(constructor),
            _ => None,
        });
        let parameters = constructor
            .map(|constructor| constructor.parameters.as_slice())
            .unwrap_or(&[]);
        self.fn_boundary += 1;
        self.function_depth += 1;
        self.push();
        self.function_roots.push(self.bound.len() - 1);
        self.preseed_parameters(parameters);
        if let Some(constructor) = constructor {
            self.preseed_vars(&constructor.body.data().statements);
            self.predeclare_immediate(&constructor.body.data().statements, false);
        }
        self.scan_parameter_initializers(parameters);
        if let Some(constructor) = constructor {
            for statement in &constructor.body.data().statements {
                self.scan_statement(statement);
            }
        }
        for member in &class.members {
            if let ClassMember::Property(property) = member.data()
                && !property.modifiers.is_static
                && !property.modifiers.is_abstract
                && !property.modifiers.is_declare
            {
                self.scan_property_name(&property.name);
                if let Some(initializer) = &property.initializer {
                    self.scan_expression(initializer);
                }
            }
        }
        self.function_roots.pop();
        self.pop();
        self.function_depth -= 1;
        self.fn_boundary -= 1;

        for member in &class.members {
            match member.data() {
                ClassMember::Constructor(_) => {}
                ClassMember::Method(method) => {
                    if let PropertyName::Computed(expression) = &method.name {
                        self.scan_expression(expression);
                    }
                    self.scan_function_like(&method.function);
                }
                ClassMember::Property(property) if property.modifiers.is_static => {
                    self.scan_property_name(&property.name);
                    if let Some(initializer) = &property.initializer {
                        self.scan_expression(initializer);
                    }
                }
                ClassMember::AutoAccessor(accessor) => {
                    self.scan_property_name(&accessor.name);
                    if let Some(initializer) = &accessor.initializer {
                        self.scan_expression(initializer);
                    }
                }
                ClassMember::StaticBlock(block) => {
                    self.push();
                    self.predeclare_immediate(&block.data().statements, false);
                    for statement in &block.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                }
                _ => {}
            }
        }
        self.pop();
    }

    fn scan_expression(&mut self, expression: &Expr) {
        match expression.data() {
            Expression::Identifier(identifier) => {
                if let Some(name) = identifier_name(self.file, identifier) {
                    self.use_name(&name);
                }
            }
            Expression::This => {
                if self.fn_boundary == 0 {
                    self.uses_this = true;
                }
            }
            Expression::Super => {}
            Expression::Meta(MetaProperty::NewTarget) => {
                if self.fn_boundary == 0 {
                    self.uses_new_target = true;
                }
            }
            Expression::Meta(MetaProperty::ImportMeta) => {}
            Expression::Literal(_) => {}
            Expression::Template(template) => {
                for expression in &template.expressions {
                    self.scan_expression(expression);
                }
            }
            Expression::TaggedTemplate(tagged) => {
                self.scan_expression(&tagged.tag);
                for expression in &tagged.template.expressions {
                    self.scan_expression(expression);
                }
            }
            Expression::Array(array) => {
                for element in &array.elements {
                    match element {
                        ArrayElement::Expression(expression) => self.scan_expression(expression),
                        ArrayElement::Spread(spread) => self.scan_expression(&spread.argument),
                        _ => {}
                    }
                }
            }
            Expression::Object(object) => {
                for member in &object.members {
                    match member.data() {
                        ObjectMember::Property(property) => {
                            if let PropertyName::Computed(key) = &property.name {
                                self.scan_expression(key);
                            }
                            self.scan_expression(&property.value);
                        }
                        ObjectMember::Method(method) => {
                            if let PropertyName::Computed(key) = &method.name {
                                self.scan_expression(key);
                            }
                            self.scan_function_like(&method.function);
                        }
                        ObjectMember::Spread(spread) => self.scan_expression(&spread.argument),
                        ObjectMember::Missing(_) => {}
                    }
                }
            }
            Expression::Function(function) => self.scan_function_like(&function.function),
            Expression::Class(class) => {
                self.scan_class_heritage(&class.class);
                self.scan_class(&class.class);
            }
            Expression::Arrow(arrow) => self.scan_arrow(arrow),
            Expression::Call(call) => {
                self.scan_expression(&call.callee);
                for argument in &call.arguments {
                    match argument {
                        CallArgument::Expression(expression) => self.scan_expression(expression),
                        CallArgument::Spread(spread) => self.scan_expression(&spread.argument),
                        CallArgument::Missing(_) => {}
                    }
                }
            }
            Expression::New(new) => {
                self.scan_expression(&new.callee);
                for argument in &new.arguments {
                    match argument {
                        CallArgument::Expression(expression) => self.scan_expression(expression),
                        CallArgument::Spread(spread) => self.scan_expression(&spread.argument),
                        CallArgument::Missing(_) => {}
                    }
                }
            }
            Expression::Member(member) => {
                self.scan_expression(&member.object);
                if let MemberProperty::Computed(expression) = &member.property {
                    self.scan_expression(expression);
                }
                if let MemberProperty::Private(private) = &member.property
                    && let Some(name) = private_name(self.file, private)
                {
                    self.use_name(&name);
                }
            }
            Expression::Await(await_expression) => self.scan_expression(&await_expression.argument),
            Expression::Yield(yield_expression) => {
                if let Some(argument) = &yield_expression.argument {
                    self.scan_expression(argument);
                }
            }
            Expression::Unary(unary) => self.scan_expression(&unary.argument),
            Expression::Update(update) => self.scan_assignment_target(&update.argument),
            Expression::Binary(binary) => {
                self.scan_expression(&binary.left);
                self.scan_expression(&binary.right);
            }
            Expression::Logical(logical) => {
                self.scan_expression(&logical.left);
                self.scan_expression(&logical.right);
            }
            Expression::Conditional(conditional) => {
                self.scan_expression(&conditional.test);
                self.scan_expression(&conditional.consequent);
                self.scan_expression(&conditional.alternate);
            }
            Expression::Assignment(assignment) => {
                self.scan_expression(&assignment.right);
                self.scan_assignment_target(&assignment.left);
            }
            Expression::Sequence(sequence) => {
                for expression in &sequence.expressions {
                    self.scan_expression(expression);
                }
            }
            Expression::Parenthesized(inner)
            | Expression::NonNull(crate::syntax::NonNullExpression { expression: inner }) => {
                self.scan_expression(inner);
            }
            Expression::As(expression) => self.scan_expression(&expression.expression),
            Expression::Satisfies(expression) => self.scan_expression(&expression.expression),
            Expression::TypeAssertion(expression) => self.scan_expression(&expression.expression),
            Expression::Import(import) => {
                self.scan_expression(&import.source);
                if let Some(options) = &import.options {
                    self.scan_expression(options);
                }
            }
            Expression::Missing(_) => {}
        }
    }

    fn scan_assignment_target(&mut self, target: &AssignmentTargetNode) {
        match target.data() {
            AssignmentTarget::Identifier(identifier) => {
                if let Some(name) = identifier_name(self.file, identifier) {
                    self.use_name(&name);
                }
            }
            AssignmentTarget::Member(member) => {
                self.scan_expression(&member.object);
                if let MemberProperty::Computed(expression) = &member.property {
                    self.scan_expression(expression);
                }
                if let MemberProperty::Private(private) = &member.property
                    && let Some(name) = private_name(self.file, private)
                {
                    self.use_name(&name);
                }
            }
            AssignmentTarget::Object(object) => {
                for property in &object.properties {
                    if let PropertyName::Computed(key) = &property.name {
                        self.scan_expression(key);
                    }
                    if let Some(initializer) = &property.initializer {
                        self.scan_expression(initializer);
                    }
                    self.scan_assignment_target(&property.target);
                }
            }
            AssignmentTarget::Array(array) => {
                for element in &array.elements {
                    if let AssignmentArrayElement::Target(inner) = element {
                        self.scan_assignment_target(inner);
                    }
                }
            }
            AssignmentTarget::Missing(_) => {}
        }
    }
}

/// The `for` head range used for the back-edge jump's diagnostic anchor.
fn head_range(for_statement: &ForStatement) -> TextRange {
    for_statement
        .test
        .as_ref()
        .map_or_else(zero_range, |test| test.range())
}

fn collect_immediate_declarations<'a>(
    file: &SourceFile,
    statements: &'a [Stmt],
) -> Vec<ImmediateDeclaration<'a>> {
    let mut declarations = Vec::new();
    for statement in statements {
        collect_immediate_declaration(file, statement, &mut declarations);
    }
    declarations
}

fn collect_immediate_declaration<'a>(
    file: &SourceFile,
    statement: &'a Stmt,
    declarations: &mut Vec<ImmediateDeclaration<'a>>,
) {
    match statement.data() {
        Statement::Variable(declaration)
            if matches!(declaration.kind, VariableKind::Let | VariableKind::Const) =>
        {
            for declarator in &declaration.declarations {
                collect_pattern_declarations(file, &declarator.data().binding, declarations);
            }
        }
        Statement::Function(declaration) => {
            let function = &declaration.function;
            if function.body.is_some()
                && let Some(identifier) = &function.name
                && let Some(name) = identifier_name(file, identifier)
            {
                declarations.push(ImmediateDeclaration {
                    name,
                    site: binding_site(identifier.range()),
                    range: statement.range(),
                    kind: ImmediateDeclarationKind::Function(function),
                });
            }
        }
        Statement::Class(class) => {
            if let Some(identifier) = &class.name
                && let Some(name) = identifier_name(file, identifier)
            {
                declarations.push(ImmediateDeclaration {
                    name,
                    site: binding_site(identifier.range()),
                    range: identifier.range(),
                    kind: ImmediateDeclarationKind::Lexical,
                });
            }
        }
        Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
            declaration,
        ))) => collect_immediate_declaration(file, declaration, declarations),
        Statement::Export(ExportDeclaration::Default(default)) => match &default.value {
            ExportDefaultValue::Function(function) => {
                if function.body.is_some()
                    && let Some(identifier) = &function.name
                    && let Some(name) = identifier_name(file, identifier)
                {
                    declarations.push(ImmediateDeclaration {
                        name,
                        site: binding_site(identifier.range()),
                        range: statement.range(),
                        kind: ImmediateDeclarationKind::Function(function),
                    });
                }
            }
            ExportDefaultValue::Class(class) => {
                if let Some(identifier) = &class.name
                    && let Some(name) = identifier_name(file, identifier)
                {
                    declarations.push(ImmediateDeclaration {
                        name,
                        site: binding_site(identifier.range()),
                        range: identifier.range(),
                        kind: ImmediateDeclarationKind::Lexical,
                    });
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn collect_pattern_declarations<'a>(
    file: &SourceFile,
    pattern: &'a Pattern,
    declarations: &mut Vec<ImmediateDeclaration<'a>>,
) {
    match pattern.data() {
        BindingPattern::Identifier(identifier) => {
            if let Some(name) = identifier_name(file, identifier) {
                declarations.push(ImmediateDeclaration {
                    name,
                    site: binding_site(identifier.range()),
                    range: identifier.range(),
                    kind: ImmediateDeclarationKind::Lexical,
                });
            }
        }
        BindingPattern::Object(object) => {
            for property in &object.properties {
                collect_pattern_declarations(file, &property.binding, declarations);
            }
        }
        BindingPattern::Array(array) => {
            for element in &array.elements {
                if let ArrayBindingElement::Binding(binding) = element {
                    collect_pattern_declarations(file, binding, declarations);
                }
            }
        }
        BindingPattern::Assignment(assignment) => {
            collect_pattern_declarations(file, &assignment.left, declarations);
        }
        BindingPattern::Rest(rest) => {
            collect_pattern_declarations(file, &rest.argument, declarations);
        }
        BindingPattern::Missing(_) => {}
    }
}

/// The runtime names a statement declares (for `export` linkage).
pub(crate) fn declared_names(file: &SourceFile, statement: &Stmt) -> Vec<String> {
    let mut names = Vec::new();
    match statement.data() {
        Statement::Variable(declaration) => {
            for declarator in &declaration.declarations {
                collect_pattern_names(file, &declarator.data().binding, &mut names);
            }
        }
        Statement::Function(declaration) => {
            if let Some(name) = &declaration.function.name
                && let Some(text) = identifier_name(file, name)
            {
                names.push(text);
            }
        }
        Statement::Class(class) => {
            if let Some(name) = &class.name
                && let Some(text) = identifier_name(file, name)
            {
                names.push(text);
            }
        }
        _ => {}
    }
    names
}

/// Collects every `var`-scoped binding name in a statement list, not descending
/// into nested function or class bodies (which have their own `var` scope).
pub(crate) fn collect_var_names(file: &SourceFile, statements: &[Stmt], names: &mut Vec<String>) {
    for statement in statements {
        collect_var_names_stmt(file, statement, names);
    }
}

fn collect_var_names_stmt(file: &SourceFile, statement: &Stmt, names: &mut Vec<String>) {
    match statement.data() {
        Statement::Variable(declaration) if matches!(declaration.kind, VariableKind::Var) => {
            for declarator in &declaration.declarations {
                collect_pattern_names(file, &declarator.data().binding, names);
            }
        }
        Statement::Block(block) => collect_var_names(file, &block.data().statements, names),
        Statement::If(statement) => {
            collect_var_names_stmt(file, &statement.consequent, names);
            if let Some(alternate) = &statement.alternate {
                collect_var_names_stmt(file, alternate, names);
            }
        }
        Statement::For(statement) => {
            if let Some(ForInitializer::Variable(declaration)) = &statement.initializer
                && matches!(declaration.kind, VariableKind::Var)
            {
                for declarator in &declaration.declarations {
                    collect_pattern_names(file, &declarator.data().binding, names);
                }
            }
            collect_var_names_stmt(file, &statement.body, names);
        }
        Statement::ForIn(statement) => {
            collect_for_binding_var(file, &statement.binding, names);
            collect_var_names_stmt(file, &statement.body, names);
        }
        Statement::ForOf(statement) => {
            collect_for_binding_var(file, &statement.binding, names);
            collect_var_names_stmt(file, &statement.body, names);
        }
        Statement::While(statement) => collect_var_names_stmt(file, &statement.body, names),
        Statement::DoWhile(statement) => collect_var_names_stmt(file, &statement.body, names),
        Statement::Switch(statement) => {
            for case in &statement.cases {
                for statement in &case.data().consequent {
                    collect_var_names_stmt(file, statement, names);
                }
            }
        }
        Statement::Try(statement) => {
            for statement in &statement.block.data().statements {
                collect_var_names_stmt(file, statement, names);
            }
            if let Some(handler) = &statement.handler {
                for statement in &handler.data().body.data().statements {
                    collect_var_names_stmt(file, statement, names);
                }
            }
            if let Some(finalizer) = &statement.finalizer {
                for statement in &finalizer.data().statements {
                    collect_var_names_stmt(file, statement, names);
                }
            }
        }
        Statement::Labeled(statement) => collect_var_names_stmt(file, &statement.body, names),
        Statement::Export(ExportDeclaration::Named(ExportNamedDeclaration::Declaration(
            statement,
        ))) => collect_var_names_stmt(file, statement, names),
        _ => {}
    }
}

fn collect_for_binding_var(file: &SourceFile, binding: &ForBinding, names: &mut Vec<String>) {
    if let ForBinding::Variable(declaration) = binding
        && matches!(declaration.kind, VariableKind::Var)
    {
        for declarator in &declaration.declarations {
            collect_pattern_names(file, &declarator.data().binding, names);
        }
    }
}

pub(crate) fn collect_pattern_names(file: &SourceFile, pattern: &Pattern, names: &mut Vec<String>) {
    match pattern.data() {
        BindingPattern::Identifier(identifier) => {
            if let Some(text) = identifier_name(file, identifier) {
                names.push(text);
            }
        }
        BindingPattern::Object(object) => {
            for property in &object.properties {
                collect_pattern_names(file, &property.binding, names);
            }
        }
        BindingPattern::Array(array) => {
            for element in &array.elements {
                if let ArrayBindingElement::Binding(inner) = element {
                    collect_pattern_names(file, inner, names);
                }
            }
        }
        BindingPattern::Rest(rest) => collect_pattern_names(file, &rest.argument, names),
        BindingPattern::Assignment(assignment) => {
            collect_pattern_names(file, &assignment.left, names);
        }
        BindingPattern::Missing(_) => {}
    }
}

/// The raw text of an identifier node, or `None` for a missing token.
fn identifier_name(file: &SourceFile, identifier: &IdentifierNode) -> Option<String> {
    let token = identifier.data().token();
    if token.is_missing() {
        return None;
    }
    file.token_text(token).map(str::to_owned)
}

/// The raw text of a private identifier (`#name`), or `None` for a missing
/// token.
fn private_name(file: &SourceFile, private: &PrivateIdentifierNode) -> Option<String> {
    let token = private.data().token();
    if token.is_missing() {
        return None;
    }
    file.token_text(token).map(str::to_owned)
}

/// Maps a source binary operator to its bytecode counterpart.
fn map_binary_operator(operator: BinaryOperator) -> BinaryOp {
    match operator {
        BinaryOperator::Add => BinaryOp::Add,
        BinaryOperator::Subtract => BinaryOp::Subtract,
        BinaryOperator::Multiply => BinaryOp::Multiply,
        BinaryOperator::Divide => BinaryOp::Divide,
        BinaryOperator::Remainder => BinaryOp::Remainder,
        BinaryOperator::Exponentiate => BinaryOp::Exponent,
        BinaryOperator::BitAnd => BinaryOp::BitAnd,
        BinaryOperator::BitOr => BinaryOp::BitOr,
        BinaryOperator::BitXor => BinaryOp::BitXor,
        BinaryOperator::LeftShift => BinaryOp::ShiftLeft,
        BinaryOperator::SignedRightShift => BinaryOp::ShiftRight,
        BinaryOperator::UnsignedRightShift => BinaryOp::UnsignedShiftRight,
        BinaryOperator::Equal => BinaryOp::Equal,
        BinaryOperator::NotEqual => BinaryOp::NotEqual,
        BinaryOperator::StrictEqual => BinaryOp::StrictEqual,
        BinaryOperator::StrictNotEqual => BinaryOp::StrictNotEqual,
        BinaryOperator::LessThan => BinaryOp::LessThan,
        BinaryOperator::LessThanOrEqual => BinaryOp::LessThanOrEqual,
        BinaryOperator::GreaterThan => BinaryOp::GreaterThan,
        BinaryOperator::GreaterThanOrEqual => BinaryOp::GreaterThanOrEqual,
        BinaryOperator::Instanceof => BinaryOp::InstanceOf,
        BinaryOperator::In => BinaryOp::In,
    }
}

/// A compound assignment's underlying operation.
enum CompoundOp {
    Arithmetic(BinaryOp),
    Logical(LogicalOperator),
}

/// The operation a compound assignment applies, or `None` for plain `=`.
fn compound_operator(operator: AssignmentOperator) -> Option<CompoundOp> {
    let op = match operator {
        AssignmentOperator::Assign => return None,
        AssignmentOperator::AddAssign => BinaryOp::Add,
        AssignmentOperator::SubtractAssign => BinaryOp::Subtract,
        AssignmentOperator::MultiplyAssign => BinaryOp::Multiply,
        AssignmentOperator::DivideAssign => BinaryOp::Divide,
        AssignmentOperator::RemainderAssign => BinaryOp::Remainder,
        AssignmentOperator::ExponentiateAssign => BinaryOp::Exponent,
        AssignmentOperator::LeftShiftAssign => BinaryOp::ShiftLeft,
        AssignmentOperator::SignedRightShiftAssign => BinaryOp::ShiftRight,
        AssignmentOperator::UnsignedRightShiftAssign => BinaryOp::UnsignedShiftRight,
        AssignmentOperator::BitAndAssign => BinaryOp::BitAnd,
        AssignmentOperator::BitOrAssign => BinaryOp::BitOr,
        AssignmentOperator::BitXorAssign => BinaryOp::BitXor,
        AssignmentOperator::LogicalAndAssign => {
            return Some(CompoundOp::Logical(LogicalOperator::And));
        }
        AssignmentOperator::LogicalOrAssign => {
            return Some(CompoundOp::Logical(LogicalOperator::Or));
        }
        AssignmentOperator::NullishAssign => {
            return Some(CompoundOp::Logical(LogicalOperator::Nullish));
        }
    };
    Some(CompoundOp::Arithmetic(op))
}

/// The canonical integer `ToString` of a numeric property key.
fn numeric_key_text(
    context: &FunctionContext<'_>,
    number: &NumericLiteralNode,
) -> Result<String, LowerError> {
    let range = number.range();
    let token = number.data().token();
    if token.is_missing() {
        return Err(context.missing(range, NodeKind::NumericLiteral));
    }
    let lexeme = context
        .file
        .token_text(token)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| context.error(range, LowerErrorKind::InvalidNumericLiteral))?;
    let value = cook_number(lexeme)
        .ok_or_else(|| context.error(range, LowerErrorKind::InvalidNumericLiteral))?;
    if value.fract() == 0.0 && value.is_finite() && (0.0..=9_007_199_254_740_991.0).contains(&value)
    {
        Ok(format!("{}", value as u64))
    } else {
        // Non-integer numeric keys stringify by their number value.
        Ok(format_number_key(value))
    }
}

fn format_number_key(value: f64) -> String {
    if value == 0.0 {
        "0".to_owned()
    } else {
        let text = format!("{value}");
        text
    }
}

/// Trims the delimiter characters from a template element's raw lexeme.
fn trim_template_delimiters(text: &str, kind: TokenKind) -> &str {
    let (head, tail): (usize, usize) = match kind {
        // `` `...` `` -> strip 1 and 1.
        TokenKind::NoSubstitutionTemplate => (1, 1),
        // `` `...${ `` -> strip 1 and 2.
        TokenKind::TemplateHead => (1, 2),
        // `}...${` -> strip 1 and 2.
        TokenKind::TemplateMiddle => (1, 2),
        // `}...` `` -> strip 1 and 1.
        TokenKind::TemplateTail => (1, 1),
        _ => (0, 0),
    };
    let bytes = text.len();
    if bytes < head + tail {
        return "";
    }
    &text[head..bytes - tail]
}

/// Cooks JavaScript escape sequences in a string/template interior. Malformed
/// escapes degrade to their literal characters rather than failing.
fn cook_escapes(input: &str) -> EcmaString {
    if !input.contains('\\') {
        return EcmaString::from_utf8(input);
    }
    let mut output = EcmaStringBuilder::with_capacity(input.encode_utf16().count());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output
                .push_code_point(u32::from(ch))
                .expect("a Rust char is a Unicode scalar");
            continue;
        }
        let Some(escape) = chars.next() else {
            output.push_unit(b'\\'.into());
            break;
        };
        match escape {
            'n' => output.push_unit(b'\n'.into()),
            't' => output.push_unit(b'\t'.into()),
            'r' => output.push_unit(b'\r'.into()),
            'b' => output.push_unit(0x0008),
            'f' => output.push_unit(0x000C),
            'v' => output.push_unit(0x000B),
            '0' if !chars.peek().is_some_and(|c| c.is_ascii_digit()) => output.push_unit(0),
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            'x' => {
                let hi = chars.next();
                let lo = chars.next();
                if let (Some(hi), Some(lo)) = (hi, lo)
                    && let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16))
                {
                    output.push_unit((h * 16 + l) as u16);
                } else {
                    output.push_unit(b'x'.into());
                }
            }
            'u' => cook_unicode_escape(&mut chars, &mut output),
            other => output
                .push_code_point(u32::from(other))
                .expect("a Rust char is a Unicode scalar"),
        }
    }
    output.finish()
}

fn cook_unicode_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    output: &mut EcmaStringBuilder,
) {
    if chars.peek() == Some(&'{') {
        chars.next();
        let mut value = 0u32;
        let mut any = false;
        while let Some(&c) = chars.peek() {
            if c == '}' {
                chars.next();
                break;
            }
            let Some(digit) = c.to_digit(16) else { break };
            value = value.saturating_mul(16).saturating_add(digit);
            any = true;
            chars.next();
        }
        if any && value <= 0x10_FFFF {
            output
                .push_code_point(value)
                .expect("a bounded code point is representable");
        }
        return;
    }
    let mut value = 0u16;
    let mut count = 0;
    while count < 4 {
        let Some(&c) = chars.peek() else { break };
        let Some(digit) = c.to_digit(16) else { break };
        value = value * 16 + digit as u16;
        chars.next();
        count += 1;
    }
    if count == 4 {
        output.push_unit(value);
    } else {
        output.push_unit(b'u'.into());
    }
}

/// Splits a regex literal `/pattern/flags` into `(pattern, flags)`.
fn split_regex(lexeme: &str) -> Option<(String, String)> {
    let lexeme = lexeme.strip_prefix('/')?;
    let last_slash = lexeme.rfind('/')?;
    let pattern = &lexeme[..last_slash];
    let flags = &lexeme[last_slash + 1..];
    Some((pattern.to_owned(), flags.to_owned()))
}
const MAX_BIGINT_CONVERSION_LIMB_OPS: usize = 1 << 24;

/// Failure while canonicalizing BigInt source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BigIntTextError {
    Invalid,
    Bytes,
    Work,
}

/// Canonicalizes a BigInt lexeme to bounded decimal text.
fn canonical_bigint_text(
    lexeme: &str,
    max_bytes: usize,
    max_limb_ops: usize,
) -> Result<String, BigIntTextError> {
    const LIMB_BASE: u64 = 1_000_000_000;
    const DECIMAL_LIMB_DIGITS: usize = 9;
    const LOG10_2_UPPER_NUMERATOR: usize = 30_103;
    const LOG10_2_UPPER_DENOMINATOR: usize = 100_000;

    let literal = lexeme.strip_suffix('n').ok_or(BigIntTextError::Invalid)?;
    let (digits, radix) = literal
        .strip_prefix("0x")
        .or_else(|| literal.strip_prefix("0X"))
        .map(|digits| (digits, 16))
        .or_else(|| {
            literal
                .strip_prefix("0o")
                .or_else(|| literal.strip_prefix("0O"))
                .map(|digits| (digits, 8))
        })
        .or_else(|| {
            literal
                .strip_prefix("0b")
                .or_else(|| literal.strip_prefix("0B"))
                .map(|digits| (digits, 2))
        })
        .unwrap_or((literal, 10));
    if digits.is_empty() {
        return Err(BigIntTextError::Invalid);
    }

    let mut previous_was_digit = false;
    let mut significant_digits = 0_usize;
    let mut first_significant_bits = 0_usize;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return Err(BigIntTextError::Invalid);
            }
            previous_was_digit = false;
            continue;
        }
        let digit = character.to_digit(radix).ok_or(BigIntTextError::Invalid)?;
        previous_was_digit = true;
        if first_significant_bits == 0 {
            if digit == 0 {
                continue;
            }
            first_significant_bits = (u32::BITS - digit.leading_zeros()) as usize;
        }
        significant_digits = significant_digits
            .checked_add(1)
            .ok_or(BigIntTextError::Work)?;
    }
    if !previous_was_digit {
        return Err(BigIntTextError::Invalid);
    }

    if radix == 10 {
        let output_bytes = significant_digits.max(1);
        if output_bytes > max_bytes {
            return Err(BigIntTextError::Bytes);
        }
        let mut output = String::with_capacity(output_bytes);
        let mut significant = false;
        for character in digits.chars().filter(|character| *character != '_') {
            if character != '0' || significant {
                significant = true;
                output.push(character);
            }
        }
        if output.is_empty() {
            output.push('0');
        }
        return Ok(output);
    }

    let bit_length = significant_digits
        .saturating_sub(1)
        .checked_mul(radix.trailing_zeros() as usize)
        .and_then(|remaining| remaining.checked_add(first_significant_bits))
        .ok_or(BigIntTextError::Work)?;
    let max_decimal_bytes = if bit_length == 0 {
        1
    } else {
        bit_length
            .checked_mul(LOG10_2_UPPER_NUMERATOR)
            .and_then(|value| value.checked_add(LOG10_2_UPPER_DENOMINATOR - 1))
            .map(|value| value / LOG10_2_UPPER_DENOMINATOR)
            .ok_or(BigIntTextError::Work)?
    };
    if max_decimal_bytes > max_bytes {
        return Err(BigIntTextError::Bytes);
    }
    let max_limb_count = max_decimal_bytes.div_ceil(DECIMAL_LIMB_DIGITS);
    let worst_case_limb_ops = significant_digits
        .checked_mul(max_limb_count)
        .ok_or(BigIntTextError::Work)?;
    if worst_case_limb_ops > max_limb_ops {
        return Err(BigIntTextError::Work);
    }

    let mut limbs = vec![0_u32];
    let mut significant = false;
    for character in digits.chars().filter(|character| *character != '_') {
        let digit = u64::from(
            character
                .to_digit(radix)
                .expect("validated BigInt digit remains valid"),
        );
        if digit == 0 && !significant {
            continue;
        }
        significant = true;
        let mut carry = digit;
        for limb in &mut limbs {
            let value = u64::from(*limb) * u64::from(radix) + carry;
            *limb = (value % LIMB_BASE) as u32;
            carry = value / LIMB_BASE;
        }
        if carry != 0 {
            limbs.push(carry as u32);
        }
    }

    let mut output = limbs.pop().ok_or(BigIntTextError::Invalid)?.to_string();
    for limb in limbs.iter().rev() {
        use std::fmt::Write as _;
        write!(output, "{limb:09}").expect("writing to String cannot fail");
    }
    if output.len() > max_bytes {
        return Err(BigIntTextError::Bytes);
    }
    Ok(output)
}

/// Cooks a scanned numeric lexeme into its ECMAScript number value.
fn cook_number(lexeme: &str) -> Option<f64> {
    let cleaned: String = lexeme.chars().filter(|c| *c != '_').collect();
    if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return radix_value(rest, 16);
    }
    if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        return radix_value(rest, 8);
    }
    if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        return radix_value(rest, 2);
    }
    cleaned.parse::<f64>().ok()
}

fn radix_value(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0.0_f64;
    for ch in digits.chars() {
        let digit = ch.to_digit(radix)?;
        value = value * f64::from(radix) + f64::from(digit);
    }
    Some(value)
}

/// Chooses the canonical pool representation for one number value.
fn number_constant(value: f64) -> Constant {
    if value.fract() == 0.0
        && value.is_finite()
        && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value)
        && !(value == 0.0 && value.is_sign_negative())
    {
        Constant::Int32(value as i32)
    } else {
        Constant::Number(NumberBits::from_f64(value))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{
        BigIntTextError, LowerErrorKind, LowerOptions, UnsupportedConstruct, canonical_bigint_text,
        cook_escapes, lower,
    };
    use crate::parser::parse;
    use crate::scanner::scan;
    use crate::source::{ScriptKind, SourceId, SourceText};

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root is readable")
    }

    fn script_kind(path: &str) -> ScriptKind {
        if path.ends_with(".d.ts")
            || path.ends_with(".ts")
            || path.ends_with(".mts")
            || path.ends_with(".cts")
        {
            ScriptKind::TypeScript
        } else if path.ends_with(".tsx") {
            ScriptKind::TypeScriptReact
        } else if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".cjs") {
            ScriptKind::JavaScript
        } else if path.ends_with(".jsx") {
            ScriptKind::JavaScriptReact
        } else {
            panic!("declared corpus source has unsupported extension: {path}");
        }
    }

    /// Discovers exactly the 20 manifest entrypoints plus 43 project sources.
    /// The checked corpus format uses one quoted `entrypoint` per manifest
    /// project and one quoted-string `source_files` array per project spec.
    fn declared_corpus_sources(root: &Path) -> Vec<String> {
        let manifest = fs::read_to_string(root.join("corpus/manifest.toml"))
            .expect("corpus manifest is readable");
        let mut sources = BTreeSet::new();
        for line in manifest.lines() {
            if let Some(value) = quoted_value(line, "entrypoint") {
                sources.insert(value);
            }
        }
        let specs = root.join("corpus/specs");
        for entry in fs::read_dir(&specs).expect("corpus specs directory is readable") {
            let path = entry.expect("spec directory entry").path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("corpus spec is UTF-8");
            let start = text
                .find("source_files")
                .unwrap_or_else(|| panic!("{} has no source_files", path.display()));
            let array = &text[start..];
            let open = array
                .find('[')
                .expect("source_files has an opening bracket");
            let close = array[open + 1..]
                .find(']')
                .map(|index| open + 1 + index)
                .expect("source_files has a closing bracket");
            let contents = &array[open + 1..close];
            for item in contents.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                let value = item
                    .strip_prefix('"')
                    .and_then(|item| item.strip_suffix('"'))
                    .unwrap_or_else(|| {
                        panic!(
                            "{} has malformed source_files item `{item}`",
                            path.display()
                        )
                    });
                sources.insert(value.to_owned());
            }
        }
        assert_eq!(
            sources.len(),
            63,
            "the checked corpus contract is 63 sources"
        );
        sources.into_iter().collect()
    }

    fn quoted_value(line: &str, key: &str) -> Option<String> {
        let line = line.trim();
        let value = line
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?
            .trim();
        Some(value.strip_prefix('"')?.strip_suffix('"')?.to_owned())
    }

    #[test]
    fn cooking_preserves_lone_surrogate_units() {
        assert_eq!(cook_escapes("\\uD800").as_units(), [0xD800]);
        assert_eq!(cook_escapes("\\uD83D\\uDE03").as_units(), [0xD83D, 0xDE03]);
        assert_eq!(cook_escapes("\\u{1F603}").as_units(), [0xD83D, 0xDE03]);
    }

    #[test]
    fn all_declared_corpus_sources_lower_to_verified_modules() {
        let root = repository_root();
        let sources = declared_corpus_sources(&root);
        let mut failures = Vec::new();
        for (index, relative) in sources.iter().enumerate() {
            let path = root.join(relative);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
            let source = Arc::new(SourceText::new(text));
            let scanned = scan(SourceId::new(index as u32), script_kind(relative), source);
            let parsed = parse(scanned);
            match lower(
                parsed.product(),
                LowerOptions {
                    javascript_compatibility: true,
                },
            ) {
                Ok(module) => {
                    assert!(
                        module.certificate(module.entry()).is_some(),
                        "{relative}: entry is verified"
                    );
                }
                Err(error) => failures.push(format!("{relative}: {error}")),
            }
        }
        assert!(
            failures.is_empty(),
            "{}/{} declared corpus sources failed lowering:\n{}",
            failures.len(),
            sources.len(),
            failures.join("\n")
        );
    }

    use bamts_bytecode::{
        BinaryOp, Constant, DecodeLimits, Instruction, Module, Register, Verified, decode_verified,
    };

    fn lower_js(src: &str) -> Module<Verified> {
        let source = Arc::new(SourceText::new(src.to_owned()));
        let scanned = scan(SourceId::new(0), ScriptKind::TypeScript, source);
        let parsed = parse(scanned);
        lower(
            parsed.product(),
            LowerOptions {
                javascript_compatibility: true,
            },
        )
        .expect("snippet lowers to a verified module")
    }

    #[test]
    fn debugger_statement_lowers_to_no_runtime_instruction() {
        let module = lower_js("debugger;");
        assert_eq!(module.functions()[0].code(), &[Instruction::Halt]);
    }

    #[test]
    fn lowering_preserves_lone_surrogate_escapes() {
        let module = lower_js("const lone = '\\uD800'; const face = '\\u{1F603}';");
        let strings: Vec<_> = module
            .constants()
            .iter()
            .filter_map(|constant| match constant {
                Constant::String(value) => Some(value.as_units()),
                _ => None,
            })
            .collect();
        assert!(strings.iter().any(|units| *units == [0xD800]));
        assert!(strings.iter().any(|units| *units == [0xD83D, 0xDE03]));
    }

    #[test]
    fn non_decimal_bigint_literals_lower_to_decimal_constants() {
        let module = lower_js(
            "const hex = 0x100000000000000000000000000000001n; \
             const octal = 0o20n; \
             const binary = 0b1_0000n;",
        );
        let bigints: Vec<_> = module
            .constants()
            .iter()
            .filter_map(|constant| match constant {
                Constant::BigInt(value) => Some(value.as_str()),
                _ => None,
            })
            .collect();
        assert!(bigints.contains(&"340282366920938463463374607431768211457"));
        assert!(bigints.contains(&"16"));
    }

    #[test]
    fn malformed_non_decimal_bigint_separator_fails_lowering() {
        let source = Arc::new(SourceText::new("const value = 0x1_n;".to_owned()));
        let scanned = scan(SourceId::new(0), ScriptKind::TypeScript, source);
        let parsed = parse(scanned);
        let error = lower(
            parsed.product(),
            LowerOptions {
                javascript_compatibility: true,
            },
        )
        .expect_err("a trailing BigInt separator is invalid");
        assert_eq!(error.kind, LowerErrorKind::InvalidBigIntLiteral);
    }

    #[test]
    fn non_decimal_bigint_conversion_honors_output_limit() {
        assert_eq!(
            canonical_bigint_text("0xffn", 3, usize::MAX).as_deref(),
            Ok("255")
        );
        assert_eq!(
            canonical_bigint_text("0xffn", 2, usize::MAX),
            Err(BigIntTextError::Bytes)
        );
    }

    #[test]
    fn non_decimal_bigint_conversion_honors_work_limit() {
        assert_eq!(
            canonical_bigint_text("0xffffn", 5, 4).as_deref(),
            Ok("65535")
        );
        assert_eq!(
            canonical_bigint_text("0xffffn", 5, 3),
            Err(BigIntTextError::Work)
        );
    }

    #[test]
    fn non_decimal_bigint_conversion_matches_small_integer_values() {
        for value in 0_u32..4096 {
            let expected = value.to_string();
            for lexeme in [
                format!("0x{value:x}n"),
                format!("0o{value:o}n"),
                format!("0b{value:b}n"),
            ] {
                assert_eq!(
                    canonical_bigint_text(&lexeme, 16, usize::MAX).as_deref(),
                    Ok(expected.as_str()),
                    "{lexeme}"
                );
            }
        }
    }

    fn any_instruction(
        module: &Module<Verified>,
        predicate: impl Fn(&Instruction) -> bool,
    ) -> bool {
        module
            .functions()
            .iter()
            .flat_map(|function| function.code())
            .any(predicate)
    }

    fn max_capture_count(module: &Module<Verified>) -> u32 {
        module
            .functions()
            .iter()
            .map(|function| function.capture_count())
            .max()
            .unwrap_or(0)
    }

    fn created_cell_used_as_capture(code: &[Instruction]) -> Option<Register> {
        code.iter().find_map(|instruction| {
            let Instruction::ArrayPush { array, value } = instruction else {
                return None;
            };
            if array == value {
                return None;
            }
            code.iter()
                .any(|candidate| {
                    matches!(
                        candidate,
                        Instruction::CreateArray { dst } | Instruction::CreateCell { dst }
                            if dst == value
                    )
                })
                .then_some(*value)
        })
    }

    fn cell_capture_pushes(code: &[Instruction], cell: Register) -> usize {
        code.iter()
            .filter(|instruction| {
                matches!(instruction, Instruction::ArrayPush { value, .. } if *value == cell)
            })
            .count()
    }

    fn cell_allocations(code: &[Instruction], cell: Register) -> usize {
        code.iter()
            .filter(|instruction| {
                matches!(
                    instruction,
                    Instruction::CreateArray { dst } | Instruction::CreateCell { dst }
                        if *dst == cell
                )
            })
            .count()
    }

    fn dormant_promotion_cell_blocks(code: &[Instruction]) -> usize {
        code.iter()
            .enumerate()
            .filter(|(entry, instruction)| {
                let Instruction::CreateArray { dst } = instruction else {
                    return false;
                };
                matches!(
                    code.get((*entry).saturating_sub(1)),
                    Some(Instruction::Jump { target }) if target.get() as usize == *entry + 2
                ) && matches!(
                    code.get(*entry + 1),
                    Some(Instruction::ArrayPush { array, value }) if array == dst && value == dst
                ) && !code.iter().any(|instruction| {
                    matches!(
                        instruction,
                        Instruction::Jump { target }
                            | Instruction::JumpIfTrue { target, .. }
                            | Instruction::JumpIfFalse { target, .. }
                            if target.get() as usize == *entry
                    )
                })
            })
            .count()
    }

    #[test]
    fn outer_write_is_observed_by_a_captured_read() {
        let module = lower_js(
            "function outer() { let value = 0; const read = () => value; value = 1; return read; }",
        );
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| {
                created_cell_used_as_capture(code).is_some()
                    && code
                        .iter()
                        .any(|instruction| matches!(instruction, Instruction::SetProperty { .. }))
            })
            .expect("outer function owns the promoted binding");
        let cell = created_cell_used_as_capture(code).expect("cell is captured");
        assert!(code.iter().any(
            |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell)
        ));
        assert!(module.functions().iter().any(|function| {
            function.capture_count() == 1
                && function.code().iter().any(
                    |instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == Register::new(0))
                )
        }));
    }

    #[test]
    fn inner_write_is_observed_by_the_outer_read() {
        let module = lower_js(
            "function outer() { let value = 0; const write = () => { value = 1; }; write(); return value; }",
        );
        assert!(module.functions().iter().any(|function| {
            function.capture_count() == 1
                && function.code().iter().any(
                    |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == Register::new(0))
                )
        }));
        assert!(module.functions().iter().any(|function| {
            let code = function.code();
            created_cell_used_as_capture(code).is_some_and(|cell| {
                code.iter().any(
                    |instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == cell)
                )
            })
        }));
    }

    #[test]
    fn assigned_local_cell_dominates_a_later_conditional_closure() {
        let module = lower_js(
            "function outer(flag) { let value = 0; value = 1; let read; if (flag) read = () => value; return value; }",
        );
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| created_cell_used_as_capture(code).is_some())
            .expect("outer function owns the captured cell");
        let cell = created_cell_used_as_capture(code).expect("cell is captured");
        let allocation = code
            .iter()
            .position(
                |instruction| matches!(instruction, Instruction::CreateArray { dst } | Instruction::CreateCell { dst } if *dst == cell),
            )
            .expect("cell allocation is present");
        let conditional = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::JumpIfFalse { .. }))
            .expect("if statement branches");
        assert!(allocation < conditional);
        assert!(code[..conditional].iter().any(
            |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell)
        ));
    }

    #[test]
    fn assigned_parameter_uses_one_cell_before_later_method_capture() {
        let module = lower_js(
            "function mitt(all) { all = all || new Map(); return { read() { return all; } }; }",
        );
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| created_cell_used_as_capture(code).is_some())
            .expect("mitt function owns the parameter cell");
        let cell = created_cell_used_as_capture(code).expect("parameter cell is captured");
        let allocation = code
            .iter()
            .position(
                |instruction| matches!(instruction, Instruction::CreateArray { dst } | Instruction::CreateCell { dst } if *dst == cell),
            )
            .expect("parameter cell is allocated");
        let assignment = code
            .iter()
            .position(
                |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell),
            )
            .expect("parameter assignment stores through its cell");
        let closure = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
            .expect("method closure is created");
        assert!(allocation < assignment && assignment < closure);
        assert_eq!(cell_allocations(code, cell), 1);
    }

    #[test]
    fn captured_parameter_cells_precede_default_initializer_closures() {
        for source in [
            "function f(a = () => a) { return a; }",
            "function f(make = () => later, later = 1) { return make; }",
            "function f(make = () => rest, ...rest) { return make; }",
            "function f({ value = () => value } = {}) { return value; }",
        ] {
            let module = lower_js(source);
            let owner = module
                .functions()
                .iter()
                .find(|function| created_cell_used_as_capture(function.code()).is_some())
                .expect("parameter owner materializes a captured default closure");
            let code = owner.code();
            let cell = created_cell_used_as_capture(code).expect("parameter cell is captured");
            let allocation = code
                .iter()
                .position(
                    |instruction| matches!(instruction, Instruction::CreateArray { dst } | Instruction::CreateCell { dst } if *dst == cell),
                )
                .expect("parameter cell is allocated");
            let closure = code
                .iter()
                .position(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
                .expect("default initializer creates a closure");
            assert!(allocation < closure, "{source}");
            assert_eq!(cell_allocations(code, cell), 1, "{source}");
        }
    }

    #[test]
    fn declaration_owned_closures_capture_their_predeclared_cells() {
        for source in [
            "function outer() { const f = () => f; return f; }",
            "function outer() { function f() { return f; } return f; }",
            "function outer() { class C { self() { return C; } } return C; }",
            "function outer() { const { f = () => f } = {}; return f; }",
            "function outer() { const f = () => f; { const f = 1; } return f; }",
        ] {
            let module = lower_js(source);
            let owner = module
                .functions()
                .iter()
                .find(|function| created_cell_used_as_capture(function.code()).is_some())
                .expect("declaration owner materializes the captured cell");
            let code = owner.code();
            let cell = created_cell_used_as_capture(code).expect("cell is captured");
            let allocation = code
                .iter()
                .position(
                    |instruction| matches!(instruction, Instruction::CreateArray { dst } | Instruction::CreateCell { dst } if *dst == cell),
                )
                .expect("captured cell is allocated");
            let closure = code
                .iter()
                .position(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
                .expect("declaration value creates a closure");
            assert!(allocation < closure, "{source}");
            assert_eq!(cell_allocations(code, cell), 1, "{source}");
            assert!(
                code[closure..].iter().any(
                    |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell)
                ),
                "the declaration stores its final value into the same cell: {source}"
            );
        }
    }

    #[test]
    fn safe_uncaptured_lexical_stays_in_a_register() {
        let module = lower_js("function outer() { const value = 1; return value; }");
        assert!(module.functions().iter().all(|function| {
            !function
                .code()
                .iter()
                .any(|instruction| matches!(instruction, Instruction::CreateCell { .. }))
        }));
    }

    #[test]
    fn captured_later_lexical_is_predeclared_once() {
        let module = lower_js(
            "function outer() { const read = () => later; const later = 1; return read; }",
        );
        let owner = module
            .functions()
            .iter()
            .find(|function| {
                function
                    .code()
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::CreateCell { .. }))
            })
            .expect("owner predeclares the later lexical cell");
        assert_eq!(
            owner
                .code()
                .iter()
                .filter(|instruction| matches!(instruction, Instruction::CreateCell { .. }))
                .count(),
            1
        );
        let cell = created_cell_used_as_capture(owner.code()).expect("closure captures the cell");
        assert_eq!(cell_allocations(owner.code(), cell), 1);
    }

    #[test]
    fn early_closure_read_uses_one_predeclared_cell_then_initializes_it() {
        let module = lower_js(
            "function outer() { const read = () => later; read(); let later = 1; return read(); }",
        );
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| created_cell_used_as_capture(code).is_some())
            .expect("outer owns the captured later binding");
        let cell = created_cell_used_as_capture(code).expect("later binding is a cell");
        let create = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::CreateCell { dst } if *dst == cell))
            .expect("cell is seeded at scope entry");
        let closure = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
            .expect("reader closure is instantiated");
        let initialize = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell))
            .expect("declaration initializes the cell");
        assert!(create < closure && closure < initialize);
        assert_eq!(cell_allocations(code, cell), 1);
    }

    #[test]
    fn function_declaration_is_instantiated_once_before_executable_statements() {
        let module = lower_js(
            "function outer() { return declaredLater(); function declaredLater() { return 2; } }",
        );
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| {
                code.iter()
                    .any(|instruction| matches!(instruction, Instruction::Call { .. }))
            })
            .expect("outer function calls its declaration");
        let closure = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
            .expect("function declaration is instantiated");
        let call = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Call { .. }))
            .expect("call is emitted");
        assert!(closure < call);
        assert_eq!(
            code.iter()
                .filter(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn class_heritage_reads_its_own_uninitialized_cell() {
        let module = lower_js("function outer() { class C extends C {} return C; }");
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| {
                code.iter()
                    .any(|instruction| matches!(instruction, Instruction::CreateCell { .. }))
            })
            .expect("class owner predeclares its binding");
        let cell = code
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::CreateCell { dst } => Some(*dst),
                _ => None,
            })
            .expect("class cell exists");
        let create = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::CreateCell { dst } if *dst == cell))
            .expect("class cell allocation");
        let heritage_read = code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == cell))
            .expect("heritage reads through the class cell");
        let initialize = code
            .iter()
            .rposition(|instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == cell))
            .expect("class declaration initializes the same cell");
        assert!(create < heritage_read && heritage_read < initialize);
    }

    #[test]
    fn same_name_shadow_does_not_overbox_uncaptured_binding() {
        let module =
            lower_js("function outer() { let value = 1; { let value = 2; } return () => value; }");
        let code = module
            .functions()
            .iter()
            .map(|function| function.code())
            .find(|code| created_cell_used_as_capture(code).is_some())
            .expect("outer binding is captured");
        assert_eq!(
            code.iter()
                .filter(|instruction| matches!(
                    instruction,
                    Instruction::CreateArray { .. } | Instruction::CreateCell { .. }
                ))
                .count(),
            2,
            "only the captured binding cell and closure capture array allocate"
        );
        assert_eq!(dormant_promotion_cell_blocks(code), 0);
    }

    #[test]
    fn sibling_getter_and_setter_capture_the_same_cell() {
        let module = lower_js(
            "function outer() { let value = 0; const get = () => value; const set = (next) => { value = next; }; return [get, set]; }",
        );
        assert!(module.functions().iter().any(|function| {
            let code = function.code();
            created_cell_used_as_capture(code)
                .is_some_and(|cell| cell_capture_pushes(code, cell) == 2)
        }));
    }

    #[test]
    fn transitive_capture_passes_an_existing_cell_without_wrapping_it() {
        let module = lower_js("function outer() { let value = 1; return () => () => value; }");
        let middle = module
            .functions()
            .iter()
            .find(|function| {
                function.capture_count() == 1
                    && function
                        .code()
                        .iter()
                        .any(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
            })
            .expect("middle closure materializes the inner closure");
        assert_eq!(
            middle
                .code()
                .iter()
                .filter(|instruction| matches!(instruction, Instruction::CreateArray { .. }))
                .count(),
            1,
            "only the capture array is allocated; capture register zero is already a cell"
        );
        assert!(middle.code().iter().any(
            |instruction| matches!(instruction, Instruction::ArrayPush { value, .. } if *value == Register::new(0))
        ));
        assert!(module.functions().iter().any(|function| {
            function.capture_count() == 1
                && !function
                    .code()
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
                && function.code().iter().any(
                    |instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == Register::new(0))
                )
        }));
    }

    #[test]
    fn capture_of_capture_reads_and_reexports_the_same_cell() {
        let module = lower_js(
            "function outer() { let value = 1; return () => { value; return () => value; }; }",
        );
        let middle = module
            .functions()
            .iter()
            .find(|function| {
                function.capture_count() == 1
                    && function
                        .code()
                        .iter()
                        .any(|instruction| matches!(instruction, Instruction::CreateClosure { .. }))
            })
            .expect("middle closure reads and reexports the capture");
        assert!(middle.code().iter().any(
            |instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == Register::new(0))
        ));
        assert_eq!(
            middle
                .code()
                .iter()
                .filter(|instruction| matches!(instruction, Instruction::CreateArray { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn logical_assignment_reads_and_writes_cell_contents() {
        let module = lower_js(
            "function outer() { let optional = false; return (term) => { optional ||= term; }; }",
        );
        assert!(module.functions().iter().any(|function| {
            function.capture_count() == 1
                && function.code().iter().any(
                    |instruction| matches!(instruction, Instruction::GetProperty { object, .. } if *object == Register::new(0))
                )
                && function.code().iter().any(
                    |instruction| matches!(instruction, Instruction::SetProperty { object, .. } if *object == Register::new(0))
                )
        }));
    }

    #[test]
    fn classic_for_let_rebinds_but_for_var_reuses_its_cell() {
        let lexical = lower_js(
            "function outer() { const reads = []; for (let index = 0; index < 2; index++) reads.push(() => index); return reads; }",
        );
        let shared = lower_js(
            "function outer() { const reads = []; for (var index = 0; index < 2; index++) reads.push(() => index); return reads; }",
        );
        let lexical_allocations = lexical
            .functions()
            .iter()
            .find_map(|function| {
                let code = function.code();
                let cell = created_cell_used_as_capture(code)?;
                Some(cell_allocations(code, cell))
            })
            .expect("lexical loop captures its binding cell");
        let shared_allocations = shared
            .functions()
            .iter()
            .find_map(|function| {
                let code = function.code();
                let cell = created_cell_used_as_capture(code)?;
                Some(cell_allocations(code, cell))
            })
            .expect("var loop captures its binding cell");
        assert_eq!(lexical_allocations, 2, "let copies into a fresh cell");
        assert_eq!(shared_allocations, 1, "var retains one function cell");
        for module in [&lexical, &shared] {
            let code = module
                .functions()
                .iter()
                .map(|function| function.code())
                .find(|code| created_cell_used_as_capture(code).is_some())
                .expect("captured loop owns a cell");
            assert_eq!(
                dormant_promotion_cell_blocks(code),
                0,
                "captured loops contain no late-promotion scaffold"
            );
        }
    }

    #[test]
    fn uncaptured_ordinary_and_loop_locals_emit_no_cell_scaffolds() {
        let ordinary = lower_js("function outer(value) { let copy = value; return copy; }");
        let classic =
            lower_js("function outer(limit) { for (let index = 0; index < limit; index++) {} }");
        let iterator = lower_js("function outer(values) { for (let value of values) {} }");

        for (label, module) in [
            ("ordinary", &ordinary),
            ("classic", &classic),
            ("iterator", &iterator),
        ] {
            let entry = module.entry().get() as usize;
            let code = module
                .functions()
                .iter()
                .enumerate()
                .find(|(index, _)| *index != entry)
                .map(|(_, function)| function.code())
                .expect("snippet has one declared function");
            assert_eq!(dormant_promotion_cell_blocks(code), 0);
            assert!(
                !code
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::CreateArray { .. })),
                "{label} local storage allocates no cell"
            );
            assert!(
                !code
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::ArrayPush { .. })),
                "{label} local storage initializes no cell"
            );
        }
    }

    #[test]
    fn iterator_let_allocates_inside_the_loop_but_var_allocates_before_it() {
        fn allocation_and_step(module: &Module<Verified>) -> (usize, usize) {
            module
                .functions()
                .iter()
                .find_map(|function| {
                    let code = function.code();
                    let cell = created_cell_used_as_capture(code)?;
                    let allocation = code.iter().position(
                        |instruction| matches!(instruction, Instruction::CreateArray { dst } | Instruction::CreateCell { dst } if *dst == cell),
                    )?;
                    let step = code
                        .iter()
                        .position(|instruction| matches!(instruction, Instruction::IteratorNext { .. }))?;
                    Some((allocation, step))
                })
                .expect("iterator loop captures its declaration")
        }

        let lexical = lower_js(
            "function outer(values) { const reads = []; for (let value of values) reads.push(() => value); return reads; }",
        );
        let shared = lower_js(
            "function outer(values) { const reads = []; for (var value of values) reads.push(() => value); return reads; }",
        );
        let (lexical_allocation, lexical_step) = allocation_and_step(&lexical);
        let (shared_allocation, shared_step) = allocation_and_step(&shared);
        assert!(
            lexical_step < lexical_allocation,
            "let creates a new cell after each iterator step"
        );
        assert!(
            shared_allocation < shared_step,
            "var creates one cell before iterator stepping begins"
        );
    }

    #[test]
    fn computed_member_access_uses_a_register_key() {
        let module = lower_js("const o: any = {}; const k = \"a\"; const v = o[k];");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::GetProperty { .. }
        )));
    }

    #[test]
    fn spread_call_builds_an_arguments_array_with_extend() {
        let module = lower_js("declare const f: any; const xs = [1]; f(...xs);");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::ArrayExtend { .. }
        )));
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::Call { .. }
        )));
    }

    #[test]
    fn nonempty_array_pushes_elements() {
        let module = lower_js("const a = [1, 2, 3];");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::ArrayPush { .. }
        )));
    }

    #[test]
    fn closure_capturing_a_local_emits_a_nonempty_capture() {
        let module = lower_js("function outer() { const x = 1; return () => x; }");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::CreateClosure { .. }
        )));
        assert!(max_capture_count(&module) >= 1, "the arrow captures `x`");
    }

    #[test]
    fn ordinary_function_declarations_materialize_own_prototypes() {
        let module = lower_js("function Base() {}");
        let code = module.functions()[0].code();
        let constants = module.constants();

        // Resolve a key register back to the exact string the lowering interned
        // for it, by walking the LoadConst that defines it and reading the
        // constant pool. This ties a SetProperty key to a property name rather
        // than matching instruction text.
        let key_name = |register: Register| -> String {
            let id = code
                .iter()
                .find_map(|instruction| match instruction {
                    Instruction::LoadConst { dst, constant } if *dst == register => Some(*constant),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no LoadConst defines key register {register:?}"));
            match &constants[id.get() as usize] {
                Constant::String(value) => value
                    .to_utf8_strict()
                    .expect("compiler-interned property key is well-formed UTF-16"),
                other => panic!("expected a string constant for the key, got {other:?}"),
            }
        };

        let (closure_index, closure) = code
            .iter()
            .enumerate()
            .find_map(|(index, instruction)| match instruction {
                Instruction::CreateClosure { dst, .. } => Some((index, *dst)),
                _ => None,
            })
            .expect("function declaration materializes a closure");

        let (prototype_index, prototype) = code[closure_index + 1..]
            .iter()
            .enumerate()
            .find_map(|(offset, instruction)| match instruction {
                Instruction::CreateObject { dst } => Some((closure_index + 1 + offset, *dst)),
                _ => None,
            })
            .expect("ordinary function gets an own prototype object");

        // P.constructor = F: the prototype is the object, the closure is the
        // value, and the key resolves to the exact string "constructor".
        let constructor_key = code[prototype_index + 1..]
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::SetProperty { object, key, value }
                    if *object == prototype && *value == closure =>
                {
                    Some(*key)
                }
                _ => None,
            })
            .expect("prototype.constructor is assigned the closure");
        assert_eq!(
            key_name(constructor_key),
            "constructor",
            "the reverse link is stored under the key \"constructor\""
        );

        // F.prototype = P: the closure is the object, the prototype is the
        // value, and the key resolves to the exact string "prototype".
        let prototype_key = code[closure_index + 1..]
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::SetProperty { object, key, value }
                    if *object == closure && *value == prototype =>
                {
                    Some(*key)
                }
                _ => None,
            })
            .expect("closure.prototype is assigned the prototype");
        assert_eq!(
            key_name(prototype_key),
            "prototype",
            "the forward link is stored under the key \"prototype\""
        );
    }

    #[test]
    fn template_literal_lowers_to_string_concatenation() {
        let module = lower_js("const a = 1; const s = `x${a}y`;");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::Binary {
                op: bamts_bytecode::BinaryOp::Add,
                ..
            }
        )));
    }

    #[test]
    fn for_of_lowers_to_the_iterator_protocol() {
        let module = lower_js("for (const v of [1, 2]) { globalThis; }");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::GetIterator { .. }
        )));
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::IteratorNext { .. }
        )));
    }

    #[test]
    fn object_destructuring_reads_named_properties() {
        let module = lower_js("declare const obj: any; const { a, b } = obj;");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::GetProperty { .. }
        )));
    }

    #[test]
    fn regex_literal_lowers_to_create_regexp() {
        let module = lower_js("const r = /ab+c/gi;");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::CreateRegExp { .. }
        )));
    }

    #[test]
    fn class_with_extends_builds_prototype_chain() {
        let module = lower_js("class B {} class C extends B { m() { return 1; } }");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::SetPrototype { .. }
        )));
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::CreateClosure { .. }
        )));
    }

    #[test]
    fn derived_constructor_places_fields_between_super_and_trailing_body() {
        let module = lower_js(
            "class Base {} class Derived extends Base { field = 1; constructor() { before(); super(); after(); } }",
        );
        let constructor = module
            .functions()
            .iter()
            .find_map(|function| {
                let calls: Vec<_> = function
                    .code()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, instruction)| {
                        matches!(instruction, Instruction::Call { .. }).then_some(index)
                    })
                    .collect();
                (calls.len() == 3).then_some((function, calls))
            })
            .expect("derived constructor contains before, super, and after calls");
        let field = constructor
            .0
            .code()
            .iter()
            .enumerate()
            .find_map(|(index, instruction)| {
                matches!(instruction, Instruction::SetProperty { .. }).then_some(index)
            })
            .expect("derived field is initialized");
        assert!(constructor.1[1] < field && field < constructor.1[2]);
    }

    #[test]
    fn implicit_derived_constructor_forwards_arguments_before_fields() {
        let module = lower_js("class Base {} class Derived extends Base { field = 1; }");
        let constructor = module
            .functions()
            .iter()
            .find(|function| {
                function
                    .code()
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::ArrayExtend { .. }))
            })
            .expect("implicit derived constructor extends its arguments array");
        let call = constructor
            .code()
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Call { .. }))
            .expect("implicit derived constructor calls its parent");
        let field = constructor
            .code()
            .iter()
            .position(|instruction| matches!(instruction, Instruction::SetProperty { .. }))
            .expect("implicit derived constructor initializes fields");
        assert!(call < field);
    }

    #[test]
    fn unsupported_derived_super_shapes_fail_lowering() {
        for source in [
            "class Base {} class Derived extends Base { constructor() {} }",
            "class Base {} class Derived extends Base { constructor() { super(); super(); } }",
            "class Base {} class Derived extends Base { constructor() { if (flag) super(); } }",
            "class Base {} class Derived extends Base { constructor() { this.x = 1; super(); } }",
        ] {
            let source = Arc::new(SourceText::new(source));
            let scanned = scan(SourceId::new(0), ScriptKind::TypeScript, source);
            let parsed = parse(scanned);
            let error = lower(
                parsed.product(),
                LowerOptions {
                    javascript_compatibility: true,
                },
            )
            .expect_err("unsupported derived constructor shape fails lowering");
            assert!(matches!(
                error.kind,
                LowerErrorKind::Unsupported(
                    UnsupportedConstruct::DerivedConstructorShape
                        | UnsupportedConstruct::ThisBeforeDerivedSuper
                )
            ));
        }
    }

    #[test]
    fn private_field_creates_a_private_name() {
        let module = lower_js("class C { #x = 1; read() { return this.#x; } }");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::CreatePrivateName { .. }
        )));
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::LoadThis { .. }
        )));
    }

    #[test]
    fn generator_sets_the_flag_and_suspends() {
        let module = lower_js("function* g() { yield 1; yield 2; }");
        assert!(
            module
                .functions()
                .iter()
                .any(|function| function.flags().is_generator)
        );
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::Suspend { .. }
        )));
        assert!(
            !any_instruction(&module, |i| matches!(i, Instruction::Await { .. })),
            "yield never emits the await opcode"
        );
    }

    #[test]
    fn async_await_uses_the_await_opcode() {
        let module = lower_js("async function f(p: any) { return await p; }");
        assert!(
            module
                .functions()
                .iter()
                .any(|function| function.flags().is_async)
        );
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::Await { .. }
        )));
        assert!(
            !any_instruction(&module, |i| matches!(i, Instruction::Suspend { .. })),
            "await never emits the yield opcode"
        );
        assert_round_trips(&module);
    }

    #[test]
    fn async_generator_distinguishes_await_from_yield() {
        let module = lower_js("async function* g(p: any) { const v = await p; yield v; }");
        assert!(
            module
                .functions()
                .iter()
                .any(|function| function.flags().is_async && function.flags().is_generator)
        );
        assert!(
            any_instruction(&module, |i| matches!(i, Instruction::Await { .. })),
            "the await operand suspends with Await"
        );
        assert!(
            any_instruction(&module, |i| matches!(i, Instruction::Suspend { .. })),
            "the produced item yields with Suspend"
        );
        assert_round_trips(&module);
    }

    #[test]
    fn for_await_splits_step_await_result() {
        let module = lower_js("async function f(xs: any) { for await (const v of xs) { v; } }");
        assert!(
            any_instruction(&module, |i| matches!(i, Instruction::IteratorStep { .. })),
            "the async iterator is stepped for its raw result"
        );
        assert!(
            any_instruction(&module, |i| matches!(i, Instruction::Await { .. })),
            "the raw iterator result is awaited"
        );
        assert!(
            any_instruction(&module, |i| matches!(i, Instruction::IteratorResult { .. })),
            "the settled iterator result yields done/value"
        );
        assert!(
            !any_instruction(&module, |i| matches!(i, Instruction::IteratorNext { .. })),
            "async loops never use the fused sync step"
        );
        assert_round_trips(&module);
    }

    #[test]
    fn free_names_load_from_the_environment() {
        let module = lower_js("const keys = Object.keys({});");
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::LoadGlobal { .. }
        )));
    }

    #[test]
    fn try_finally_routes_completions_through_the_finalizer() {
        let module = lower_js(
            "function f(x: any) { while (x) { try { return 1; } finally { x = 0; } } return 2; }",
        );
        // A handler is registered and the finalizer runs before the return.
        assert!(
            module
                .functions()
                .iter()
                .any(|function| !function.handlers().is_empty())
        );
        assert_round_trips(&module);
    }

    #[test]
    fn optional_member_call_skips_arguments_and_preserves_receiver() {
        let key_is_method = |code: &[Instruction], constants: &[Constant], register: Register| {
            let constant = code
                .iter()
                .find_map(|instruction| match instruction {
                    Instruction::LoadConst { dst, constant } if *dst == register => Some(*constant),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no LoadConst defines property key {register:?}"));
            match &constants[constant.get() as usize] {
                Constant::String(value) => {
                    value.to_utf8_strict().is_ok_and(|value| value == "method")
                }
                _ => false,
            }
        };

        let assert_optional_call = |src: &str, expect_method_jump: bool| {
            let module = lower_js(src);
            let code = module.functions()[0].code();
            let constants = module.constants();

            let (member_index, callee, object) = code
                .iter()
                .enumerate()
                .find_map(|(index, instruction)| match instruction {
                    Instruction::GetProperty { dst, object, key }
                        if key_is_method(code, constants, *key) =>
                    {
                        Some((index, *dst, *object))
                    }
                    _ => None,
                })
                .expect("optional member call loads its method");

            let (call_index, call_dst, this_value) = code
                .iter()
                .enumerate()
                .find_map(|(index, instruction)| match instruction {
                    Instruction::Call {
                        dst,
                        callee: call_callee,
                        this_value,
                        ..
                    } if *call_callee == callee => Some((index, *dst, *this_value)),
                    _ => None,
                })
                .expect("optional member call invokes the loaded method");
            assert_eq!(
                this_value, object,
                "the call keeps the member object as this"
            );

            let result_move_index = code[call_index + 1..]
                .iter()
                .enumerate()
                .find_map(|(offset, instruction)| match instruction {
                    Instruction::Move { src, .. } if *src == call_dst => {
                        Some(call_index + 1 + offset)
                    }
                    _ => None,
                })
                .expect("the call result is moved into a shared result register");
            let merge = result_move_index + 1;

            let (object_jump_index, object_jump_target) = code[..member_index]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, instruction)| match instruction {
                    Instruction::JumpIfTrue { condition, target }
                        if code[..index].iter().any(|prior| {
                            matches!(
                                prior,
                                Instruction::Binary { dst, op: BinaryOp::Equal, left, .. }
                                    if *dst == *condition && *left == object
                            )
                        }) =>
                    {
                        Some((index, *target))
                    }
                    _ => None,
                })
                .expect("optional object test branches before the member read");
            assert!(
                code[object_jump_index + 1..call_index]
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::Call { .. })),
                "the argument side effect is emitted before the member call"
            );
            assert_eq!(
                object_jump_target.get() as usize,
                merge,
                "the object-nullish branch jumps to the instruction after the result assignment"
            );

            if expect_method_jump {
                let method_jump_target = code[member_index + 1..call_index]
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(offset, instruction)| {
                        let index = member_index + 1 + offset;
                        match instruction {
                            Instruction::JumpIfTrue { condition, target }
                                if code[member_index + 1..index].iter().any(|prior| {
                                    matches!(
                                        prior,
                                        Instruction::Binary { dst, op: BinaryOp::Equal, left, .. }
                                            if *dst == *condition && *left == callee
                                    )
                                }) =>
                            {
                                Some(*target)
                            }
                            _ => None,
                        }
                    })
                    .expect("optional method test branches before the call");
                assert_eq!(
                    method_jump_target.get() as usize,
                    merge,
                    "the method-nullish branch jumps to the same merge point"
                );
            }
        };

        assert_optional_call(
            "declare const obj: any; declare function side_effect(): number; obj?.method(side_effect());",
            false,
        );
        assert_optional_call(
            "declare const obj: any; declare function side_effect(): number; obj?.method?.(side_effect());",
            true,
        );
    }

    #[test]
    fn arrow_rest_loads_the_activations_own_arguments() {
        // `(...options) => options` must collect the arrow invocation's own
        // actual arguments, not an unconditional empty array. Under the old
        // `ArgumentsSource::None` branch the arrow body emitted `CreateArray`
        // with no `LoadArguments`, so rest was always empty.
        let module = lower_js("const collect = (...options) => options; collect(1, 2);");
        let entry = module.entry().get() as usize;
        let arrow = module
            .functions()
            .iter()
            .enumerate()
            .find(|(index, _)| *index != entry)
            .map(|(_, function)| function)
            .expect("the arrow is the sole non-entry function");
        let code = arrow.code();
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::LoadArguments { .. })),
            "the arrow body loads its own activation arguments for rest"
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::GetIterator { .. })),
            "rest collection iterates the loaded arguments"
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::ArrayPush { .. })),
            "rest collection pushes into the rest array"
        );
        assert_round_trips(&module);
    }

    #[test]
    fn regular_function_rest_with_fixed_parameters_loads_arguments() {
        // A regular function with fixed parameters followed by rest must still
        // load its own arguments, discard the fixed prefix, then collect.
        let module = lower_js("function f(a, ...rest) { return rest; }");
        let entry = module.entry().get() as usize;
        let function = module
            .functions()
            .iter()
            .enumerate()
            .find(|(index, _)| *index != entry)
            .map(|(_, function)| function)
            .expect("the regular function is the sole non-entry function");
        let code = function.code();
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::LoadArguments { .. })),
            "the function loads its own arguments for rest"
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::GetIterator { .. })),
            "rest collection iterates the loaded arguments"
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::IteratorNext { .. })),
            "the fixed parameter is discarded by stepping the iterator"
        );
        assert_round_trips(&module);
    }

    #[test]
    fn arrow_lexical_arguments_is_captured_not_loaded() {
        // An arrow referencing `arguments` captures it from the enclosing
        // function; the identifier read must not emit `LoadArguments` in the
        // arrow body. This invariant is independent of rest-parameter loading
        // and proves the lexical `arguments` binding is untouched.
        let module = lower_js("function outer() { const read = () => arguments; return read(); }");
        let arrow = module
            .functions()
            .iter()
            .find(|function| function.capture_count() >= 1)
            .expect("the arrow captures `arguments` from outer");
        assert!(
            !arrow
                .code()
                .iter()
                .any(|instruction| matches!(instruction, Instruction::LoadArguments { .. })),
            "the arrow reads `arguments` from its capture, not its own activation"
        );
        assert_round_trips(&module);
    }

    fn assert_round_trips(module: &Module<Verified>) {
        let bytes = module.encode();
        decode_verified(&bytes, &DecodeLimits::default())
            .expect("a verified module re-decodes and re-verifies");
    }
}
