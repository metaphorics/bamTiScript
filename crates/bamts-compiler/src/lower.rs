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
//! names, `this`/`arguments`/`new.target`, generators and async via
//! [`Instruction::Suspend`], and module exports.
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
    AccessorKind, BigIntLiteral, BinaryOp, Constant, ConstantId, ExceptionHandler, Function,
    FunctionFlags, FunctionId, Instruction, IteratorKind, MAX_CONSTANTS, MAX_FUNCTIONS,
    MAX_INSTRUCTIONS, MAX_REGISTERS, Module, NumberBits, Pc, Register, UnaryOp, Verified,
    VerifyError,
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
    ForInitializer, ForOfMode, ForOfStatement, ForStatement, FunctionBody, FunctionDeclaration,
    FunctionLike, IdentifierNode, IfStatement, ImportBinding, ImportDeclaration,
    ImportSpecifierMode, Literal, LogicalExpression, LogicalOperator, MemberExpression,
    MemberProperty, MetaProperty, ModuleExportName, NewExpression, NodeKind, NumericLiteralNode,
    ObjectLiteral, ObjectMember, ParameterNode, Pattern, PrivateIdentifierNode, PropertyModifier,
    PropertyName, RegexLiteralNode, SourceFile, Statement, Stmt, StringLiteralNode,
    SwitchStatement, TemplateElementNode, TemplateLiteral, TokenKind, UnaryOperator,
    UpdateExpression, UpdateOperator, VariableDeclaration, VariableKind, WhileStatement,
    YieldExpression,
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

/// Structural production ceilings, mirroring the bytecode verifier's limits.
/// Two instruction slots are always reserved for a function's terminating
/// epilogue so a body can never leave no room for its own terminator.
const MAX_BODY_INSTRUCTIONS: usize = MAX_INSTRUCTIONS as usize - 2;
/// Persisted string constants must fit the deterministic decode ceiling so an
/// assembled module round-trips through [`bamts_bytecode::decode`].
const MAX_STRING_BYTES: usize = 1 << 20;

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
    /// A runtime construct the current instruction set cannot express.
    Unsupported(UnsupportedConstruct),
    /// A structural production capacity ran out.
    Capacity(CapacityLimit),
    /// The assembled module failed bytecode verification. Lowering maintains
    /// every verifier invariant by construction, so this is defensive.
    Verify(VerifyError),
}

/// Runtime syntax this instruction set cannot express faithfully, or that
/// carries no runtime semantics. Every variant names one rejected construct;
/// there is no catch-all. None of these occur in the executable corpus.
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
    /// `debugger` is a host breakpoint request with no bytecode.
    DebuggerStatement,
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
    /// `import.meta` (no host meta-object primitive).
    ImportMeta,
    /// An identifier spelled with unicode escape sequences.
    EscapedIdentifier,
    /// A non-decimal (`0x`/`0o`/`0b`) bigint literal.
    NonDecimalBigInt,
    /// A `return` at module top level.
    ReturnOutsideFunction,
}

/// The exhausted structural capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLimit {
    Registers,
    Constants,
    Functions,
    Instructions,
    StringBytes,
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
            Self::DebuggerStatement => "`debugger` statement",
            Self::EnumDeclaration => "runtime `enum` declaration",
            Self::NamespaceDeclaration => "runtime `namespace` declaration",
            Self::RuntimeImportEquals => "runtime `import =` declaration",
            Self::RuntimeExportAll => "runtime `export *` declaration",
            Self::ExportAssignment => "`export =` assignment",
            Self::DecoratedDeclaration => "decorated declaration",
            Self::DynamicImportExpression => "dynamic `import()` with a non-literal specifier",
            Self::ImportMeta => "`import.meta` meta property",
            Self::EscapedIdentifier => "identifier containing escape sequences",
            Self::NonDecimalBigInt => "non-decimal bigint literal",
            Self::ReturnOutsideFunction => "top-level `return`",
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
            Self::StringBytes => "string constant exceeds the deterministic pool byte ceiling",
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
    assemble_with_linkage_strings(file, options, &[], false)
}

pub(crate) fn assemble_program_module(
    file: &SourceFile,
    options: LowerOptions,
    linkage_strings: &[String],
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    assemble_with_linkage_strings(file, options, linkage_strings, true)
}

fn assemble_with_linkage_strings(
    file: &SourceFile,
    options: LowerOptions,
    linkage_strings: &[String],
    program_mode: bool,
) -> Result<Module<bamts_bytecode::Unverified>, LowerError> {
    validate_script_kind(file, options)?;

    let mut builder = ModuleBuilder {
        source: file.source_id(),
        constants: Vec::new(),
        functions: Vec::new(),
    };
    for value in linkage_strings {
        builder.intern(Constant::String(value.clone()), file.range())?;
    }
    let entry = builder.reserve_function(file.range())?;

    let mut context = FunctionContext::new_top_level(file, program_mode);
    context.lower_top_level(&mut builder, file.statements())?;
    context.emit(file.range(), Instruction::Halt)?;
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
            && value.len() > MAX_STRING_BYTES
        {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::StringBytes)));
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

/// What a resolved name denotes inside the current function. Nested functions
/// materialize as closures at their reference site, so a resolved name always
/// names a live register home.
#[derive(Clone, Copy)]
enum Binding {
    /// A value living in a fixed register (the binding's home).
    Local(Register),
}

/// One captured cell of a nested function, in the deterministic capture order.
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
    loops: Vec<LoopFrame>,
    handlers: Vec<ExceptionHandler>,
    finally_stack: Vec<FinallyFrame>,
    /// `true` for the module entry function, whose bindings are the module
    /// environment (named globals) rather than register homes.
    top_level: bool,
    program_mode: bool,
    /// `Some(reg)` when `this` is a captured cell (arrow); `None` when the
    /// activation owns `this` (`LoadThis`).
    this_capture: Option<Register>,
    /// `Some(reg)` when `new.target` is a captured cell (arrow); `None` when
    /// owned (`LoadNewTarget`).
    new_target_capture: Option<Register>,
    arguments_source: ArgumentsSource,
}

impl<'a> FunctionContext<'a> {
    fn new_top_level(file: &'a SourceFile, program_mode: bool) -> Self {
        Self {
            file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: true,
            program_mode,
            this_capture: None,
            new_target_capture: None,
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
        value: String,
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
            .find_map(|scope| scope.get(name).cloned())
    }

    fn declare(&mut self, name: String, binding: Binding, function_scoped: bool) {
        let scope = if function_scoped {
            self.scopes
                .first_mut()
                .expect("a function context always holds its root scope")
        } else {
            self.scopes
                .last_mut()
                .expect("a function context always holds at least one scope")
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

    /// Declares a function-local binding (register home) or a module global.
    /// Returns the home register for locals, or `None` for globals (whose
    /// storage is the environment, addressed by name).
    fn declare_local(
        &mut self,
        name: &str,
        range: TextRange,
        function_scoped: bool,
    ) -> Result<Option<Register>, LowerError> {
        if self.top_level {
            // Module-scope binding: it lives in the environment.
            Ok(None)
        } else {
            let home = self.alloc_register(range)?;
            self.declare(name.to_owned(), Binding::Local(home), function_scoped);
            Ok(Some(home))
        }
    }

    /// Stores a produced value into a freshly declared binding named `name`.
    fn store_binding(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        value: Register,
        range: TextRange,
        function_scoped: bool,
    ) -> Result<(), LowerError> {
        if function_scoped
            && let Some(Binding::Local(home)) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(name).cloned())
        {
            // Reuse a hoisted `var` (or parameter) home rather than shadowing.
            return self.move_to(range, home, value);
        }
        match self.declare_local(name, range, function_scoped)? {
            Some(home) => self.move_to(range, home, value),
            None => {
                let id = builder.intern(Constant::String(name.to_owned()), range)?;
                self.emit(range, Instruction::StoreGlobal { name: id, value })?;
                Ok(())
            }
        }
    }

    /// Pre-allocates and zero-initializes register homes for every `var`-scoped
    /// binding in a function body, implementing `var` hoisting so a read on a
    /// path that bypasses the declaration observes `undefined`, not an
    /// uninitialized register. Module-scope `var`s are environment globals and
    /// need no hoisting.
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
            if matches!(
                self.scopes
                    .first()
                    .and_then(|scope| scope.get(&name).cloned()),
                Some(Binding::Local(_))
            ) {
                // Already a parameter or capture home.
                continue;
            }
            let home = self.alloc_register(range)?;
            let id = builder.intern(Constant::Undefined, range)?;
            self.emit(
                range,
                Instruction::LoadConst {
                    dst: home,
                    constant: id,
                },
            )?;
            self.declare(name, Binding::Local(home), true);
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
        if let Some(Binding::Local(register)) = self.resolve(name) {
            return Ok(register);
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
        let id = builder.intern(Constant::String(name.to_owned()), range)?;
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
        if let Some(Binding::Local(home)) = self.resolve(name) {
            return self.move_to(range, home, value);
        }
        let id = builder.intern(Constant::String(name.to_owned()), range)?;
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
        for statement in statements {
            self.lower_statement(builder, statement)?;
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
                if import.is_type_only {
                    Ok(())
                } else {
                    Err(self.unsupported(range, UnsupportedConstruct::RuntimeImportEquals))
                }
            }
            Statement::Export(export) => self.lower_export(builder, range, export),
            Statement::Variable(declaration) => {
                self.lower_variable_declaration(builder, declaration)
            }
            Statement::Function(declaration) => {
                self.lower_function_declaration(builder, range, declaration)
            }
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
                self.lower_expression(builder, &expression.expression)?;
                Ok(())
            }
            Statement::If(if_statement) => self.lower_if(builder, if_statement),
            Statement::Switch(switch) => self.lower_switch(builder, range, switch),
            Statement::For(for_statement) => self.lower_for(builder, for_statement),
            Statement::ForIn(for_in) => self.lower_for_in(builder, range, for_in),
            Statement::ForOf(for_of) => self.lower_for_of(builder, range, for_of),
            Statement::While(while_statement) => self.lower_while(builder, while_statement),
            Statement::DoWhile(do_while) => self.lower_do_while(builder, do_while),
            Statement::Try(try_statement) => self.lower_try(builder, range, try_statement),
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
            Statement::Debugger => {
                Err(self.unsupported(range, UnsupportedConstruct::DebuggerStatement))
            }
            Statement::Missing(missing) => Err(self.missing(range, missing.expected())),
        }
    }

    fn lower_block(
        &mut self,
        builder: &mut ModuleBuilder,
        block: &Block,
    ) -> Result<(), LowerError> {
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
        if let Some(initializer) = &for_statement.initializer {
            match initializer {
                ForInitializer::Variable(declaration) => {
                    self.lower_variable_declaration(builder, declaration)?;
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
    /// `for await`/`of`.
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
                let function_scoped = matches!(declaration.kind, VariableKind::Var);
                let declarator = declaration
                    .declarations
                    .first()
                    .ok_or_else(|| self.missing(range, NodeKind::VariableDeclarator))?;
                self.bind_pattern(builder, &declarator.data().binding, value, function_scoped)
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
        self.push_scope();
        let clause = handler_clause.data();
        if let Some(binding) = &clause.binding {
            let bind_result = self.bind_pattern(builder, binding, catch_register, false);
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
                self.push_scope();
                let clause = handler_clause.data();
                if let Some(binding) = &clause.binding {
                    let bind_result = self.bind_pattern(builder, binding, catch_register, false);
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
        let finally_result = self.lower_block(builder, finalizer.data());
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
        self.emit(range, Instruction::Return { value: value_reg })?;
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // throw
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_THROW)?;
        self.emit(range, Instruction::Throw { value: value_reg })?;
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // break
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_BREAK)?;
        let break_jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        match self.loops.last_mut() {
            Some(frame) => frame.breaks.push(break_jump),
            None => {
                let target = self.next_pc();
                self.patch_jump(break_jump, target);
            }
        }
        let after = self.next_pc();
        self.patch_jump(skip, after);
        // continue
        let skip = self.emit_kind_guard(builder, range, kind_reg, COMPLETION_CONTINUE)?;
        let continue_jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        match self.nearest_loop_index() {
            Some(index) => self.loops[index].continues.push(continue_jump),
            None => {
                let target = self.next_pc();
                self.patch_jump(continue_jump, target);
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
        let function_scoped = match declaration.kind {
            VariableKind::Var => true,
            VariableKind::Let | VariableKind::Const => false,
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
            let value = match &data.initializer {
                Some(initializer) => self.lower_expression(builder, initializer)?,
                None => {
                    // A bare declaration (`let x;`) binds undefined.
                    self.undefined(builder, range)?
                }
            };
            self.bind_pattern(builder, &data.binding, value, function_scoped)?;
        }
        Ok(())
    }

    fn lower_function_declaration(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        declaration: &FunctionDeclaration,
    ) -> Result<(), LowerError> {
        let function = &declaration.function;
        if function.body.is_none() {
            // A bodiless overload or ambient signature is type-only.
            return Ok(());
        }
        let name = match &function.name {
            Some(identifier) => self.identifier_text(identifier)?,
            None => return Ok(()),
        };
        let closure = self.build_function_value(builder, range, Some(name.clone()), function)?;
        self.store_binding(builder, &name, closure, range, true)
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
            Expression::Super => {
                // A bare `super` reference outside a call/member is not valid;
                // super calls/members are handled at their call/member sites.
                self.this_value(range)
            }
            Expression::Literal(literal) => self.lower_literal(builder, range, literal),
            Expression::Template(template) => self.lower_template(builder, range, template),
            Expression::TaggedTemplate(tagged) => {
                self.lower_tagged_template(builder, range, tagged)
            }
            Expression::Array(array) => self.lower_array(builder, range, array),
            Expression::Object(object) => self.lower_object(builder, range, object),
            Expression::Function(function) => {
                self.build_function_value(builder, range, None, &function.function)
            }
            Expression::Class(class) => self.lower_class_value(builder, range, &class.class, None),
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
                let id = builder.intern(Constant::String(name), range)?;
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

    /// `await x` suspends on `x` and resumes with the settled value in `dst`.
    fn lower_await(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        await_expression: &AwaitExpression,
    ) -> Result<Register, LowerError> {
        let src = self.lower_expression(builder, &await_expression.argument)?;
        self.emit_suspend(range, src)
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

    /// Emits a suspension yielding `src`, returning the register that receives
    /// the resumed value.
    fn emit_suspend(&mut self, range: TextRange, src: Register) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        let resume = Pc::new(self.code.len() as u32 + 1);
        self.emit(range, Instruction::Suspend { dst, src, resume })?;
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
        let raw_key = self.string_reg(builder, "raw".to_owned(), range)?;
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

    fn cooked_template_parts(&self, template: &TemplateLiteral) -> Result<Vec<String>, LowerError> {
        template
            .elements
            .iter()
            .map(|element| self.template_element_text(element, true))
            .collect()
    }

    fn raw_template_parts(&self, template: &TemplateLiteral) -> Result<Vec<String>, LowerError> {
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
    ) -> Result<String, LowerError> {
        let token = element.data().token();
        if token.is_missing() {
            return Ok(String::new());
        }
        let Some(text) = self.file.token_text(token) else {
            return Ok(String::new());
        };
        let interior = trim_template_delimiters(text, token.kind());
        if cook {
            Ok(cook_escapes(interior))
        } else {
            Ok(interior.to_owned())
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
        let pattern_id = builder.intern(Constant::String(pattern), range)?;
        let flags_id = builder.intern(Constant::String(flags), range)?;
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
        let canonical = canonical_bigint_text(lexeme)
            .ok_or_else(|| self.unsupported(range, UnsupportedConstruct::NonDecimalBigInt))?;
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

    fn string_literal_value(&self, string: &StringLiteralNode) -> Result<String, LowerError> {
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
                self.string_reg(builder, name, identifier.range())
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
                self.string_reg(builder, text, identifier.range())
            }
            PropertyName::String(string) => {
                let value = self.string_literal_value(string)?;
                self.string_reg(builder, value, string.range())
            }
            PropertyName::Number(number) => {
                let key = numeric_key_text(self, number)?;
                self.string_reg(builder, key, number.range())
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
        if import.type_only || self.program_mode {
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
            self.store_binding(builder, &name, value, range, true)?;
        }
        match &clause.binding {
            Some(ImportBinding::Namespace(identifier)) => {
                let name = self.identifier_text(identifier)?;
                self.store_binding(builder, &name, module, range, true)?;
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
                    self.store_binding(builder, &local, value, range, true)?;
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
        // `import("literal")` loads the module namespace; a non-literal
        // specifier has no static linkage entry.
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
        let key = self.string_reg(builder, name.to_owned(), range)?;
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
        if self.program_mode {
            return Ok(());
        }
        let src = self.read_name(builder, local, range)?;
        let name = builder.intern(Constant::String(exported.to_owned()), range)?;
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
        if self.program_mode {
            debug_assert_eq!(exported, "default");
            return self.store_binding(builder, "*default*", src, range, false);
        }
        let name = builder.intern(Constant::String(exported.to_owned()), range)?;
        self.emit(range, Instruction::Export { name, src })?;
        Ok(())
    }

    fn lower_export(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        export: &ExportDeclaration,
    ) -> Result<(), LowerError> {
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
                if *type_only || self.program_mode {
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
                if all.type_only || self.program_mode {
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
                    let name = match &function.name {
                        Some(identifier) => Some(self.identifier_text(identifier)?),
                        None => None,
                    };
                    let closure =
                        self.build_function_value(builder, range, name.clone(), function)?;
                    if let Some(name) = &name {
                        self.store_binding(builder, name, closure, range, true)?;
                    }
                    self.export_value(builder, range, "default", closure)
                }
                ExportDefaultValue::Class(class) => {
                    let value = self.lower_class_value(builder, range, class, None)?;
                    if let Some(identifier) = &class.name {
                        let name = self.identifier_text(identifier)?;
                        self.store_binding(builder, &name, value, range, false)?;
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
            ModuleExportName::String(string) => self.string_literal_value(string),
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
        function_scoped: bool,
    ) -> Result<(), LowerError> {
        let range = pattern.range();
        match pattern.data() {
            BindingPattern::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.store_binding(builder, &name, value, range, function_scoped)
            }
            BindingPattern::Object(object) => {
                let mut taken: Vec<Register> = Vec::new();
                for property in &object.properties {
                    if let BindingPattern::Rest(rest) = property.binding.data() {
                        let rest_value = self.rest_object(builder, range, value, &taken)?;
                        self.bind_pattern(builder, &rest.argument, rest_value, function_scoped)?;
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
                    self.bind_pattern(builder, &property.binding, element, function_scoped)?;
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
                                    function_scoped,
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
                                self.bind_pattern(builder, inner, value, function_scoped)?;
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
                self.bind_pattern(builder, &assignment.left, value, function_scoped)
            }
            BindingPattern::Rest(rest) => {
                // A bare rest at the top level binds the whole value.
                self.bind_pattern(builder, &rest.argument, value, function_scoped)
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
        let mut inner = FunctionContext {
            file: self.file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: false,
            program_mode: self.program_mode,
            this_capture: None,
            new_target_capture: None,
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
                    inner.declare(name.clone(), Binding::Local(register), true);
                }
                CaptureKey::This => inner.this_capture = Some(register),
                CaptureKey::Arguments => {
                    inner.arguments_source = ArgumentsSource::Captured(register);
                }
                CaptureKey::NewTarget => inner.new_target_capture = Some(register),
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
            Some(name) => Some(builder.intern(Constant::String(name), range)?),
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
        for (index, parameter) in parameters.iter().take(fixed).enumerate() {
            let data = parameter.data();
            let slot = slots[index];
            let value = match &data.initializer {
                Some(default) => self.apply_default(builder, parameter.range(), slot, default)?,
                None => slot,
            };
            self.bind_pattern(builder, &data.binding, value, true)?;
        }
        if let Some(rest_index) = rest_index {
            let parameter = &parameters[rest_index];
            let rest_argument = match parameter.data().binding.data() {
                BindingPattern::Rest(rest) => &rest.argument,
                _ => unreachable!("rest_index points at a rest binding"),
            };
            let rest = self.collect_rest_parameter(builder, range, fixed as u32)?;
            self.bind_pattern(builder, rest_argument, rest, true)?;
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
        let arguments = match self.arguments_value(builder, range)? {
            Some(register) => register,
            None => {
                // No arguments object (arrow): rest is empty.
                let array = self.alloc_register(range)?;
                self.emit(range, Instruction::CreateArray { dst: array })?;
                return Ok(array);
            }
        };
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
            CaptureKey::Name(name) => self.read_name(builder, name, range),
            CaptureKey::This => self.this_value(range),
            CaptureKey::Arguments => match self.arguments_value(builder, range)? {
                Some(register) => Ok(register),
                None => self.undefined(builder, range),
            },
            CaptureKey::NewTarget => self.new_target_value(range),
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
            if matches!(self.resolve(name), Some(Binding::Local(_))) {
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
        let value = self.lower_class_value(builder, range, class, name.as_deref())?;
        if let Some(name) = &name {
            self.store_binding(builder, name, value, range, false)?;
        }
        Ok(())
    }

    /// Builds a class as a constructor function plus a prototype object with
    /// methods, accessors, static members, private names, and prototype chain.
    fn lower_class_value(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        class: &ClassDeclaration,
        _name: Option<&str>,
    ) -> Result<Register, LowerError> {
        if let Some(decorator) = class.decorators.first() {
            return Err(self.unsupported(
                decorator.range(),
                UnsupportedConstruct::DecoratedDeclaration,
            ));
        }
        self.push_scope();
        self.create_private_names(builder, range, class)?;
        // The parent (superclass) constructor, if any.
        let parent = match &class.extends {
            Some(heritage) => Some(self.lower_expression(builder, &heritage.expression)?),
            None => None,
        };
        // Constructor function.
        let constructor = self.find_constructor(class);
        let ctor = self.build_constructor(builder, range, class, constructor)?;
        // Prototype object.
        let prototype = self.alloc_register(range)?;
        self.emit(range, Instruction::CreateObject { dst: prototype })?;
        if let Some(parent) = parent {
            // C.prototype.__proto__ = P.prototype; C.__proto__ = P.
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
        // ctor.prototype = prototype.
        let prototype_key = self.string_reg(builder, "prototype".to_owned(), range)?;
        self.emit(
            range,
            Instruction::SetProperty {
                object: ctor,
                key: prototype_key,
                value: prototype,
            },
        )?;
        // Members.
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
                let description = builder.intern(Constant::String(text.clone()), range)?;
                let dst = self.alloc_register(range)?;
                self.emit(range, Instruction::CreatePrivateName { dst, description })?;
                self.declare(text, Binding::Local(dst), false);
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
        let captures = self.compute_constructor_captures(parameters, body_block, &fields);
        let id = builder.reserve_function(range)?;
        self.build_constructor_into(
            builder, id, range, parameters, body_block, &fields, &captures,
        )?;
        self.materialize_closure(builder, range, id, &captures)
    }

    fn compute_constructor_captures(
        &self,
        parameters: &[ParameterNode],
        body: Option<&Block>,
        fields: &[&crate::syntax::ClassProperty],
    ) -> Vec<CaptureKey> {
        let mut scanner = FreeVarScanner::new(self.file);
        scanner.bind_parameters(parameters);
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
            if matches!(self.resolve(name), Some(Binding::Local(_))) {
                captures.push(CaptureKey::Name(name.clone()));
            }
        }
        captures
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
    ) -> Result<(), LowerError> {
        let mut inner = FunctionContext {
            file: self.file,
            code: Vec::new(),
            registers: 0,
            capture_count: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            top_level: false,
            program_mode: self.program_mode,
            this_capture: None,
            new_target_capture: None,
            arguments_source: ArgumentsSource::Own,
        };
        for capture in captures {
            let register = inner.alloc_register(range)?;
            inner.capture_count += 1;
            if let CaptureKey::Name(name) = capture {
                inner.declare(name.clone(), Binding::Local(register), true);
            }
        }
        inner.bind_parameters(builder, parameters, range)?;
        if let Some(block) = body {
            inner.hoist_vars(builder, &block.statements, range)?;
        }
        // Instance field initializers: this[field] = init.
        for field in fields {
            let this_value = inner.this_value(range)?;
            let key = inner.property_key(builder, &field.name)?;
            let value = match &field.initializer {
                Some(initializer) => inner.lower_expression(builder, initializer)?,
                None => inner.undefined(builder, range)?,
            };
            inner.emit(
                range,
                Instruction::SetProperty {
                    object: this_value,
                    key,
                    value,
                },
            )?;
        }
        if let Some(block) = body {
            inner.lower_block(builder, block)?;
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

/// Collects the free variables and lexical `this`/`arguments`/`new.target`
/// usage of a function, for capture analysis.
struct FreeVarScanner<'a> {
    file: &'a SourceFile,
    bound: Vec<HashSet<String>>,
    free: BTreeSet<String>,
    uses_this: bool,
    uses_arguments: bool,
    uses_new_target: bool,
    /// Depth of enclosing non-arrow function boundaries; `this`/`arguments`/
    /// `new.target` inside a nested non-arrow function do not escape.
    fn_boundary: u32,
}

impl<'a> FreeVarScanner<'a> {
    fn new(file: &'a SourceFile) -> Self {
        Self {
            file,
            bound: vec![HashSet::new()],
            free: BTreeSet::new(),
            uses_this: false,
            uses_arguments: false,
            uses_new_target: false,
            fn_boundary: 0,
        }
    }

    fn scan_function(
        &mut self,
        parameters: &[ParameterNode],
        body: ArrowBody<'_>,
        _is_arrow: bool,
    ) {
        self.push();
        self.bind_parameters(parameters);
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
        self.pop();
    }

    fn push(&mut self) {
        self.bound.push(HashSet::new());
    }

    fn pop(&mut self) {
        self.bound.pop();
    }

    fn bind(&mut self, name: String) {
        if let Some(scope) = self.bound.last_mut() {
            scope.insert(name);
        }
    }

    fn is_bound(&self, name: &str) -> bool {
        self.bound.iter().any(|scope| scope.contains(name))
    }

    fn scan_property_name(&mut self, name: &PropertyName) {
        match name {
            PropertyName::Computed(expression) => self.scan_expression(expression),
            PropertyName::Private(private) => {
                if let Some(text) = private_name(self.file, private)
                    && !self.is_bound(&text)
                {
                    self.free.insert(text);
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
        if !self.is_bound(name) {
            self.free.insert(name.to_owned());
        }
    }

    fn bind_parameters(&mut self, parameters: &[ParameterNode]) {
        for parameter in parameters {
            let data = parameter.data();
            self.bind_pattern(&data.binding);
            if let Some(initializer) = &data.initializer {
                self.scan_expression(initializer);
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern) {
        match pattern.data() {
            BindingPattern::Identifier(identifier) => {
                if let Some(text) = identifier_name(self.file, identifier) {
                    self.bind(text);
                }
            }
            BindingPattern::Object(object) => {
                for property in &object.properties {
                    if let PropertyName::Computed(expression) = &property.name {
                        self.scan_expression(expression);
                    }
                    if let Some(initializer) = &property.initializer {
                        self.scan_expression(initializer);
                    }
                    self.bind_pattern(&property.binding);
                }
            }
            BindingPattern::Array(array) => {
                for element in &array.elements {
                    if let ArrayBindingElement::Binding(inner) = element {
                        self.bind_pattern(inner);
                    }
                }
            }
            BindingPattern::Rest(rest) => self.bind_pattern(&rest.argument),
            BindingPattern::Assignment(assignment) => {
                self.scan_expression(&assignment.right);
                self.bind_pattern(&assignment.left);
            }
            BindingPattern::Missing(_) => {}
        }
    }

    fn scan_statement(&mut self, statement: &Stmt) {
        match statement.data() {
            Statement::Variable(declaration) => {
                for declarator in &declaration.declarations {
                    if let Some(initializer) = &declarator.data().initializer {
                        self.scan_expression(initializer);
                    }
                    self.bind_pattern(&declarator.data().binding);
                }
            }
            Statement::Function(declaration) => {
                if let Some(name) = &declaration.function.name
                    && let Some(text) = identifier_name(self.file, name)
                {
                    self.bind(text);
                }
                self.scan_function_like(&declaration.function);
            }
            Statement::Class(class) => {
                if let Some(name) = &class.name
                    && let Some(text) = identifier_name(self.file, name)
                {
                    self.bind(text);
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
                            for declarator in &declaration.declarations {
                                if let Some(init) = &declarator.data().initializer {
                                    self.scan_expression(init);
                                }
                                self.bind_pattern(&declarator.data().binding);
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
                for statement in &statement.block.data().statements {
                    self.scan_statement(statement);
                }
                self.pop();
                if let Some(handler) = &statement.handler {
                    self.push();
                    if let Some(binding) = &handler.data().binding {
                        self.bind_pattern(binding);
                    }
                    for statement in &handler.data().body.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                }
                if let Some(finalizer) = &statement.finalizer {
                    self.push();
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
                ExportDefaultValue::Function(function) => self.scan_function_like(function),
                ExportDefaultValue::Class(class) => self.scan_class(class),
                ExportDefaultValue::Missing(_) => {}
            },
            _ => {}
        }
    }

    fn scan_for_binding(&mut self, binding: &ForBinding) {
        match binding {
            ForBinding::Variable(declaration) => {
                for declarator in &declaration.declarations {
                    self.bind_pattern(&declarator.data().binding);
                }
            }
            ForBinding::Target(target) => self.scan_assignment_target(target),
        }
    }

    fn scan_function_like(&mut self, function: &FunctionLike) {
        self.fn_boundary += 1;
        self.push();
        self.bind_parameters(&function.parameters);
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
        self.pop();
        self.fn_boundary -= 1;
    }

    fn scan_arrow(&mut self, arrow: &ArrowFunction) {
        // Arrows are lexical: do not increment fn_boundary.
        self.push();
        self.bind_parameters(&arrow.parameters);
        match &arrow.body {
            FunctionBody::Block(block) => {
                for statement in &block.data().statements {
                    self.scan_statement(statement);
                }
            }
            FunctionBody::Expression(expression) => self.scan_expression(expression),
            FunctionBody::Missing(_) => {}
        }
        self.pop();
    }

    fn scan_class(&mut self, class: &ClassDeclaration) {
        if let Some(heritage) = &class.extends {
            self.scan_expression(&heritage.expression);
        }
        for member in &class.members {
            match member.data() {
                ClassMember::Constructor(constructor) => {
                    self.fn_boundary += 1;
                    self.push();
                    self.bind_parameters(&constructor.parameters);
                    for statement in &constructor.body.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                    self.fn_boundary -= 1;
                }
                ClassMember::Method(method) => {
                    if let PropertyName::Computed(expression) = &method.name {
                        self.scan_expression(expression);
                    }
                    self.scan_function_like(&method.function);
                }
                ClassMember::Property(property) => {
                    if let PropertyName::Computed(expression) = &property.name {
                        self.scan_expression(expression);
                    }
                    if let Some(initializer) = &property.initializer {
                        self.fn_boundary += 1;
                        self.scan_expression(initializer);
                        self.fn_boundary -= 1;
                    }
                }
                ClassMember::AutoAccessor(accessor) => {
                    if let Some(initializer) = &accessor.initializer {
                        self.fn_boundary += 1;
                        self.scan_expression(initializer);
                        self.fn_boundary -= 1;
                    }
                }
                ClassMember::StaticBlock(block) => {
                    self.fn_boundary += 1;
                    self.push();
                    for statement in &block.data().statements {
                        self.scan_statement(statement);
                    }
                    self.pop();
                    self.fn_boundary -= 1;
                }
                _ => {}
            }
        }
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
            Expression::Class(class) => self.scan_class(&class.class),
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
fn cook_escapes(input: &str) -> String {
    if !input.contains('\\') {
        return input.to_owned();
    }
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let Some(escape) = chars.next() else {
            output.push('\\');
            break;
        };
        match escape {
            'n' => output.push('\n'),
            't' => output.push('\t'),
            'r' => output.push('\r'),
            'b' => output.push('\u{8}'),
            'f' => output.push('\u{c}'),
            'v' => output.push('\u{b}'),
            '0' if !chars.peek().is_some_and(|c| c.is_ascii_digit()) => output.push('\0'),
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
                    && let Some(decoded) = char::from_u32(h * 16 + l)
                {
                    output.push(decoded);
                } else {
                    output.push('x');
                }
            }
            'u' => cook_unicode_escape(&mut chars, &mut output),
            other => output.push(other),
        }
    }
    output
}

fn cook_unicode_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, output: &mut String) {
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
        if any && let Some(decoded) = char::from_u32(value) {
            output.push(decoded);
        }
        return;
    }
    let mut value = 0u32;
    let mut count = 0;
    while count < 4 {
        let Some(&c) = chars.peek() else { break };
        let Some(digit) = c.to_digit(16) else { break };
        value = value * 16 + digit;
        chars.next();
        count += 1;
    }
    if count == 4
        && let Some(decoded) = char::from_u32(value)
    {
        output.push(decoded);
    } else {
        output.push('u');
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

/// Canonicalizes a decimal bigint lexeme to canonical decimal text.
fn canonical_bigint_text(lexeme: &str) -> Option<String> {
    let digits = lexeme.strip_suffix('n')?;
    if digits.is_empty() {
        return None;
    }
    if digits.len() >= 2 {
        let prefix = &digits[..2];
        if matches!(prefix, "0x" | "0X" | "0o" | "0O" | "0b" | "0B") {
            return None;
        }
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let trimmed = cleaned.trim_start_matches('0');
    if trimmed.is_empty() {
        Some("0".to_owned())
    } else {
        Some(trimmed.to_owned())
    }
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

    use super::{LowerOptions, lower};
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

    use bamts_bytecode::{DecodeLimits, Instruction, Module, Verified, decode_verified};

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
    }

    #[test]
    fn async_await_suspends() {
        let module = lower_js("async function f(p: any) { return await p; }");
        assert!(
            module
                .functions()
                .iter()
                .any(|function| function.flags().is_async)
        );
        assert!(any_instruction(&module, |i| matches!(
            i,
            Instruction::Suspend { .. }
        )));
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

    fn assert_round_trips(module: &Module<Verified>) {
        let bytes = module.encode();
        decode_verified(&bytes, &DecodeLimits::default())
            .expect("a verified module re-decodes and re-verifies");
    }
}
