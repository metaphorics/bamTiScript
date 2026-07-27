//! Direct lowering from the runtime TypeScript/JavaScript AST to verified
//! canonical bytecode ([`bamts_bytecode`]).
//!
//! # Scope
//!
//! This lowering targets the production instruction algebra (20 opcodes,
//! `u32` register/constant/function/pc indices, exception handlers, and a
//! definite-initialization verifier). It emits *only* what that algebra can
//! express *faithfully* and rejects everything else with a typed
//! [`LowerError`] carrying the offending node's [`SourceId`] and [`TextRange`].
//! No construct is ever silently approximated: if the instruction set cannot
//! model a runtime form's semantics, that form is a typed
//! [`UnsupportedConstruct`], not a plausible-looking miscompilation.
//!
//! ## Faithfully lowered runtime kernel
//!
//! - **Constants**: number (`Int32`/canonical IEEE-754), escape-free string,
//!   boolean, `null`, `undefined`, and decimal `bigint`.
//! - **Operators**: every unary operator ([`UnaryOp`]) except `delete` of a
//!   binding; every binary operator ([`BinaryOp`]); short-circuit `&&`, `||`,
//!   and `??`; the conditional `?:`; assignment (`=` and every compound form)
//!   and update (`++`/`--`) to a binding or a static-keyed member; `void`,
//!   sequence, parenthesized, and the erased TypeScript expression wrappers
//!   (`as`, `satisfies`, `<T>`, `!`).
//! - **Control flow**: `if`/`else`, `while`, `do`/`while`, C-style `for`,
//!   `switch`, unlabeled `break`/`continue`, `return`, `throw`, and
//!   `try`/`catch` (empty or absent `finally` only).
//! - **Values**: object literals with static-keyed data properties, methods,
//!   and shorthands; empty arrays; static-keyed property read/write/delete;
//!   calls and `new` with positional arguments; non-capturing function and
//!   arrow expressions/declarations with simple identifier parameters; `await`
//!   (as [`Instruction::Suspend`]); and module `import` (as
//!   [`Instruction::Import`] plus static-keyed member reads for named/default
//!   bindings).
//!
//! ## Value and control-flow model
//!
//! Each binding owns one fixed register (its *home*); initialization and
//! assignment copy the value into that home register with an explicit
//! [`Instruction::Move`], so a binding read after a branch or across a loop
//! back-edge is provably initialized on every path — exactly what the
//! verifier's definite-initialization fixpoint requires. Branch and loop
//! targets are emitted as placeholder jumps and patched once the join PC is
//! known. Value-producing branches (`?:`, `&&`, `||`, `??`) write their result
//! into a single destination register on every path before the merge.
//!
//! ## Unexpressible-in-ISA forms
//!
//! Forms whose semantics the current instruction set cannot represent are
//! enumerated in [`UnsupportedConstruct`] with a specific variant each; see the
//! module-level rejection sites. These include register-keyed (computed)
//! property access, spread, non-empty array literals, `for`/`in` and `for`/`of`
//! iteration, destructuring, template literals, classes, generators, and
//! captured closures. They are reported so the instruction set can be extended
//! and this lowering re-run, never faked.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use bamts_bytecode::{
    BigIntLiteral, BinaryOp, Constant, ConstantId, ExceptionHandler, Function, FunctionFlags,
    FunctionId, Instruction, MAX_CONSTANTS, MAX_FUNCTIONS, MAX_INSTRUCTIONS, MAX_REGISTERS, Module,
    NumberBits, Pc, Register, UnaryOp, Verified, VerifyError,
};

use crate::source::{ScriptKind, SourceId, TextRange, Utf16Pos};
use crate::syntax::{
    ArrowFunction, AssignmentExpression, AssignmentOperator, AssignmentTarget, AwaitExpression,
    BinaryExpression, BinaryOperator, BindingPattern, Block, BooleanLiteralNode, CallArgument,
    CallExpression, ConditionalExpression, DoWhileStatement, ExportDeclaration, ExportDefaultValue,
    ExportNamedDeclaration, ExportSpecifierMode, Expr, Expression, ForInitializer, ForStatement,
    FunctionBody, FunctionDeclaration, FunctionLike, IdentifierNode, IfStatement, ImportBinding,
    ImportDeclaration, ImportSpecifierMode, Literal, LogicalExpression, LogicalOperator,
    MemberExpression, MemberProperty, ModuleExportName, NewExpression, NodeKind,
    NumericLiteralNode, ObjectLiteral, ObjectMember, Parameter, ParameterNode, PropertyName,
    SourceFile, Statement, Stmt, StringLiteralNode, SwitchStatement, TokenKind, UnaryOperator,
    UpdateExpression, UpdateOperator, VariableDeclaration, VariableKind, WhileStatement,
};

/// A degenerate range at the start of the document, used as the diagnostic
/// anchor for nodes whose own range is absent (missing syntax slots).
fn zero_range() -> TextRange {
    // `TextRange::new` is checked; `Utf16Pos::ZERO..Utf16Pos::ZERO` is always
    // ordered, so the `Ok` arm is the only reachable one.
    match TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO) {
        Ok(range) => range,
        Err(_) => unreachable!("Utf16Pos::ZERO is never after itself"),
    }
}
/// Caller-selected lowering mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LowerOptions {
    /// Accepts JavaScript [`ScriptKind`]s in addition to TypeScript ones.
    /// TypeScript sources are always accepted; JavaScript sources without
    /// this flag are a typed error.
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
    /// An identifier resolved to no binding in the enclosing function.
    UnresolvedIdentifier { name: String },
    /// An identifier resolved only in an enclosing function; the instruction
    /// set exposes no way for a closure to read an outer function's register.
    ClosureCapture { name: String },
    /// A name bound to a hoisted function was used as an assignment target; a
    /// function binding has no mutable register home.
    ReassignFunctionBinding { name: String },
    /// A runtime construct the current instruction set cannot express.
    Unsupported(UnsupportedConstruct),
    /// A structural production capacity ran out.
    Capacity(CapacityLimit),
    /// The assembled module failed bytecode verification. Lowering maintains
    /// every verifier invariant by construction, so this is defensive.
    Verify(VerifyError),
}

/// Runtime syntax this instruction set cannot express faithfully. Every variant
/// names one rejected construct; there is no catch-all. Each corresponds to a
/// specific missing instruction-set capability documented at its rejection
/// site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedConstruct {
    // -- No register-keyed property instruction ------------------------------
    /// `obj[expr]` where `expr` is not a literal string/integer key.
    ComputedMemberAccess,
    /// `obj?.x`, `obj?.[k]`, or `f?.()`.
    OptionalChaining,
    // -- No array element / spread / apply instruction -----------------------
    /// `[a, b]` with any element (only `[]` is expressible).
    ArrayElements,
    /// `...expr` in an array, object, or call.
    SpreadElement,
    /// `{ ...expr }`.
    ObjectSpread,
    // -- No computed-key / accessor definition instruction -------------------
    /// `{ [expr]: v }`.
    ComputedPropertyKey,
    /// `get`/`set` object member or a non-integer numeric key.
    AccessorProperty,
    /// A numeric property key that is not a canonical non-negative integer.
    NonIntegerPropertyKey,
    /// `#field` private name.
    PrivateField,
    // -- No iterator / enumeration protocol ----------------------------------
    ForInStatement,
    ForOfStatement,
    // -- No destructuring (needs iterator / computed-key composites) ---------
    DestructuringBinding,
    DestructuringAssignment,
    /// A parameter with a default value, rest, or destructuring pattern.
    ComplexParameter,
    // -- No string-coercion / concat guarantee -------------------------------
    TemplateLiteral,
    TaggedTemplate,
    // -- No dedicated constant / value kind ----------------------------------
    RegexLiteral,
    /// A non-decimal (`0x`/`0o`/`0b`) bigint literal.
    NonDecimalBigInt,
    /// A cooked-value pipeline is required to interpret escape sequences.
    EscapedStringLiteral,
    EscapedIdentifier,
    // -- No prototype / this / new.target semantics --------------------------
    ClassDeclaration,
    ClassExpression,
    ThisExpression,
    SuperExpression,
    MetaProperty,
    // -- No generator resume protocol ----------------------------------------
    GeneratorFunction,
    YieldExpression,
    // -- No binding-deletion / disposable / linkage instruction --------------
    DeleteBinding,
    UsingDeclaration,
    RuntimeImportEquals,
    RuntimeExportSpecifiers,
    RuntimeExportAll,
    ExportAssignment,
    ExportDefaultClass,
    // -- No abrupt-completion (finally) unwind semantics ---------------------
    TryFinally,
    // -- Expressible in principle but out of this revision's scope -----------
    WithStatement,
    LabeledStatement,
    LabeledJump,
    DebuggerStatement,
    EnumDeclaration,
    NamespaceDeclaration,
    DecoratedDeclaration,
    DynamicImport,
    /// `import.meta`-less bare side-effect import with no expressible effect.
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
    ArgumentCount,
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
            Self::UnresolvedIdentifier { name } => write!(f, "unresolved identifier `{name}`"),
            Self::ClosureCapture { name } => write!(
                f,
                "`{name}` is only bound in an enclosing function; the instruction set cannot \
                 capture an outer function's register"
            ),
            Self::ReassignFunctionBinding { name } => write!(
                f,
                "function binding `{name}` has no mutable register home to assign into"
            ),
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
            Self::ComputedMemberAccess => {
                "computed member access (no register-keyed property instruction)"
            }
            Self::OptionalChaining => "optional chaining `?.`",
            Self::ArrayElements => "array literal with elements (no array element instruction)",
            Self::SpreadElement => "spread `...` element (no spread/apply instruction)",
            Self::ObjectSpread => "object spread `{ ... }`",
            Self::ComputedPropertyKey => {
                "computed property key (no register-keyed property instruction)"
            }
            Self::AccessorProperty => "`get`/`set` accessor property",
            Self::NonIntegerPropertyKey => "non-integer numeric property key",
            Self::PrivateField => "private `#` field",
            Self::ForInStatement => "`for..in` statement (no enumeration instruction)",
            Self::ForOfStatement => "`for..of` statement (no iterator instruction)",
            Self::DestructuringBinding => "destructuring binding pattern",
            Self::DestructuringAssignment => "destructuring assignment target",
            Self::ComplexParameter => "parameter with a default, rest, or destructuring pattern",
            Self::TemplateLiteral => "template literal (no string-coercion instruction)",
            Self::TaggedTemplate => "tagged template expression",
            Self::RegexLiteral => "regular-expression literal",
            Self::NonDecimalBigInt => "non-decimal bigint literal",
            Self::EscapedStringLiteral => "string literal containing escape sequences",
            Self::EscapedIdentifier => "identifier containing escape sequences",
            Self::ClassDeclaration => "`class` declaration (no prototype/instanceof semantics)",
            Self::ClassExpression => "`class` expression (no prototype/instanceof semantics)",
            Self::ThisExpression => "`this` expression (no receiver binding)",
            Self::SuperExpression => "`super` expression",
            Self::MetaProperty => "meta property (`new.target`/`import.meta`)",
            Self::GeneratorFunction => "generator function (no resume protocol)",
            Self::YieldExpression => "`yield` expression (no resume protocol)",
            Self::DeleteBinding => "`delete` of a binding",
            Self::UsingDeclaration => "`using` declaration",
            Self::RuntimeImportEquals => "runtime `import =` declaration",
            Self::RuntimeExportSpecifiers => "runtime `export { .. }` re-export",
            Self::RuntimeExportAll => "runtime `export *` declaration",
            Self::ExportAssignment => "`export =` assignment",
            Self::ExportDefaultClass => "`export default class`",
            Self::TryFinally => "`try`/`finally` (no abrupt-completion unwind instruction)",
            Self::WithStatement => "`with` statement",
            Self::LabeledStatement => "labeled statement",
            Self::LabeledJump => "labeled `break`/`continue`",
            Self::DebuggerStatement => "`debugger` statement",
            Self::EnumDeclaration => "`enum` declaration",
            Self::NamespaceDeclaration => "`namespace` declaration",
            Self::DecoratedDeclaration => "decorated declaration",
            Self::DynamicImport => "dynamic `import()` expression",
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
            Self::ArgumentCount => "too many call arguments",
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
/// Top-level statements become the entry function; every supported function
/// declaration becomes one additional module function. The returned module has
/// passed [`Module::verify`].
///
/// # Errors
/// Returns a typed [`LowerError`] for an unsupported source kind, a parser
/// recovery node, an unexpressible runtime construct, an exhausted capacity, or
/// (defensively) a verification failure.
pub fn lower(file: &SourceFile, options: LowerOptions) -> Result<Module<Verified>, LowerError> {
    validate_script_kind(file, options)?;

    let mut builder = ModuleBuilder {
        source: file.source_id(),
        constants: Vec::new(),
        functions: Vec::new(),
        globals: HashMap::new(),
    };
    let entry = builder.reserve_function(file.range())?;

    let mut context = FunctionContext::new(file, HashSet::new(), true);
    context.hoist_functions(&mut builder, file.statements())?;
    for statement in file.statements() {
        context.lower_statement(&mut builder, statement)?;
    }
    context.emit(file.range(), Instruction::Halt)?;
    let assembled = context.into_function(None, FunctionFlags::default());
    builder.fill_function(entry, assembled);

    let functions = builder
        .functions
        .into_iter()
        .map(|slot| slot.expect("every reserved function slot is filled before assembly"))
        .collect();
    Module::new(builder.constants, functions, entry)
        .verify()
        .map_err(|error| LowerError {
            source: file.source_id(),
            range: file.range(),
            kind: LowerErrorKind::Verify(error),
        })
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

/// Module-wide constant pool, function table, and global function bindings.
struct ModuleBuilder {
    source: SourceId,
    constants: Vec<Constant>,
    functions: Vec<Option<Function>>,
    /// Top-level function declarations, resolvable from any function body
    /// without capture because a [`FunctionId`] is a stable global reference.
    globals: HashMap<String, FunctionId>,
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

/// What a resolved name denotes inside the current function.
#[derive(Clone, Copy)]
enum Binding {
    /// A value living in a fixed register (the binding's home).
    Local(Register),
    /// A hoisted function, materialized on reference via `DefineFunction`.
    Function(FunctionId),
}

/// A live loop's break/continue placeholder jumps, patched when the loop ends.
struct LoopFrame {
    breaks: Vec<Pc>,
    continues: Vec<Pc>,
}

/// Per-function lowering state: code, register allocator, and lexical scopes.
struct FunctionContext<'a> {
    file: &'a SourceFile,
    code: Vec<Instruction>,
    registers: u32,
    parameter_count: u32,
    scopes: Vec<HashMap<String, Binding>>,
    /// Names bound in enclosing functions, kept to distinguish a typed
    /// closure-capture error from a plain unresolved identifier.
    outer_names: HashSet<String>,
    loops: Vec<LoopFrame>,
    handlers: Vec<ExceptionHandler>,
    top_level: bool,
}

impl<'a> FunctionContext<'a> {
    fn new(file: &'a SourceFile, outer_names: HashSet<String>, top_level: bool) -> Self {
        Self {
            file,
            code: Vec::new(),
            registers: 0,
            parameter_count: 0,
            scopes: vec![HashMap::new()],
            outer_names,
            loops: Vec::new(),
            handlers: Vec::new(),
            top_level,
        }
    }

    fn into_function(self, name: Option<ConstantId>, flags: FunctionFlags) -> Function {
        Function::new(
            name,
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

    /// Appends one instruction, returning its program counter for patching.
    fn emit(&mut self, range: TextRange, instruction: Instruction) -> Result<Pc, LowerError> {
        if self.code.len() >= MAX_BODY_INSTRUCTIONS {
            return Err(self.error(range, LowerErrorKind::Capacity(CapacityLimit::Instructions)));
        }
        let pc = Pc::new(self.code.len() as u32);
        self.code.push(instruction);
        Ok(pc)
    }

    /// The program counter the next emitted instruction will occupy.
    fn next_pc(&self) -> Pc {
        Pc::new(self.code.len() as u32)
    }

    /// Retargets an already-emitted jump to a resolved program counter.
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

    /// Interns a constant and loads it into a fresh register.
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

    // ------------------------------------------------------------------
    // Names and scopes
    // ------------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
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

    fn resolve(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
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

    /// Every name visible here, for closure-capture classification.
    fn visible_names(&self) -> HashSet<String> {
        let mut names = self.outer_names.clone();
        for scope in &self.scopes {
            names.extend(scope.keys().cloned());
        }
        names
    }

    /// Resolves a name in value position to a register holding its value.
    fn read_binding(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &str,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        if let Some(binding) = self.resolve(name) {
            return match binding {
                Binding::Local(register) => Ok(register),
                Binding::Function(id) => self.materialize_function(range, id),
            };
        }
        if let Some(id) = builder.globals.get(name).copied() {
            return self.materialize_function(range, id);
        }
        if name == "undefined" {
            return self.load_constant(builder, Constant::Undefined, range);
        }
        if self.outer_names.contains(name) {
            return Err(self.error(
                range,
                LowerErrorKind::ClosureCapture {
                    name: name.to_owned(),
                },
            ));
        }
        Err(self.error(
            range,
            LowerErrorKind::UnresolvedIdentifier {
                name: name.to_owned(),
            },
        ))
    }

    fn materialize_function(
        &mut self,
        range: TextRange,
        function: FunctionId,
    ) -> Result<Register, LowerError> {
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::DefineFunction { dst, function })?;
        Ok(dst)
    }

    /// Resolves an assignment-target name to its mutable home register.
    fn resolve_assignable(
        &mut self,
        builder: &ModuleBuilder,
        name: &str,
        range: TextRange,
    ) -> Result<Register, LowerError> {
        match self.resolve(name) {
            Some(Binding::Local(register)) => Ok(register),
            Some(Binding::Function(_)) => Err(self.error(
                range,
                LowerErrorKind::ReassignFunctionBinding {
                    name: name.to_owned(),
                },
            )),
            None if builder.globals.contains_key(name) => Err(self.error(
                range,
                LowerErrorKind::ReassignFunctionBinding {
                    name: name.to_owned(),
                },
            )),
            None if self.outer_names.contains(name) => Err(self.error(
                range,
                LowerErrorKind::ClosureCapture {
                    name: name.to_owned(),
                },
            )),
            None => Err(self.error(
                range,
                LowerErrorKind::UnresolvedIdentifier {
                    name: name.to_owned(),
                },
            )),
        }
    }

    // ------------------------------------------------------------------
    // Function hoisting
    // ------------------------------------------------------------------

    /// Binds every hoistable function declaration in `statements` before any
    /// statement runs, so recursion and forward/mutual references resolve.
    fn hoist_functions(
        &mut self,
        builder: &mut ModuleBuilder,
        statements: &[Stmt],
    ) -> Result<(), LowerError> {
        for statement in statements {
            let declaration = match statement.data() {
                Statement::Function(declaration) => declaration,
                Statement::Export(ExportDeclaration::Named(
                    ExportNamedDeclaration::Declaration(inner),
                )) => match inner.data() {
                    Statement::Function(declaration) => declaration,
                    _ => continue,
                },
                _ => continue,
            };
            let function = &declaration.function;
            if function.body.is_none() {
                // A bodiless overload or ambient signature is type-only.
                continue;
            }
            let Some(identifier) = &function.name else {
                continue;
            };
            let name = self.identifier_text(identifier)?;
            let id = builder.reserve_function(statement.range())?;
            self.declare(name.clone(), Binding::Function(id), true);
            if self.top_level {
                builder.globals.insert(name, id);
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
            Statement::Class(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::ClassDeclaration))
            }
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
            Statement::ForIn(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::ForInStatement))
            }
            Statement::ForOf(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::ForOfStatement))
            }
            Statement::While(while_statement) => self.lower_while(builder, while_statement),
            Statement::DoWhile(do_while) => self.lower_do_while(builder, do_while),
            Statement::Try(try_statement) => self.lower_try(builder, range, try_statement),
            Statement::With(_) => Err(self.unsupported(range, UnsupportedConstruct::WithStatement)),
            Statement::Labeled(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::LabeledStatement))
            }
            Statement::Break(jump) => self.lower_break(range, jump.label.is_some()),
            Statement::Continue(jump) => self.lower_continue(range, jump.label.is_some()),
            Statement::Return(return_statement) => {
                if self.top_level {
                    return Err(
                        self.unsupported(range, UnsupportedConstruct::ReturnOutsideFunction)
                    );
                }
                let value = match &return_statement.argument {
                    Some(expression) => self.lower_expression(builder, expression)?,
                    None => self.load_constant(builder, Constant::Undefined, range)?,
                };
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
        self.hoist_functions(builder, &block.statements)?;
        for statement in &block.statements {
            self.lower_statement(builder, statement)?;
        }
        Ok(())
    }

    /// Lowers one statement as a nested lexical scope (a loop or branch body).
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

    fn lower_break(&mut self, range: TextRange, labeled: bool) -> Result<(), LowerError> {
        if labeled {
            return Err(self.unsupported(range, UnsupportedConstruct::LabeledJump));
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let frame = match self.loops.last_mut() {
            Some(frame) => frame,
            None => {
                return Err(self.error(
                    range,
                    LowerErrorKind::MissingSyntax {
                        expected: NodeKind::BreakStatement,
                    },
                ))
            }
        };
        frame.breaks.push(jump);
        Ok(())
    }

    fn lower_continue(&mut self, range: TextRange, labeled: bool) -> Result<(), LowerError> {
        if labeled {
            return Err(self.unsupported(range, UnsupportedConstruct::LabeledJump));
        }
        let jump = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let frame = match self.loops.last_mut() {
            Some(frame) => frame,
            None => {
                return Err(self.error(
                    range,
                    LowerErrorKind::MissingSyntax {
                        expected: NodeKind::ContinueStatement,
                    },
                ))
            }
        };
        frame.continues.push(jump);
        Ok(())
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
        });
        // Phase 1: emit the comparison chain. Each non-default case jumps to
        // its own body when the strict comparison holds; a final jump routes a
        // no-match to the default body (or past the switch).
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
        // Phase 2: lay out the case bodies consecutively so fall-through is the
        // natural sequential successor.
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
        // A `continue` inside a switch targets an enclosing loop, which this
        // revision cannot resolve from here; reject rather than mis-route.
        if let Some(jump) = frame.continues.first().copied() {
            let _ = jump;
            return Err(self.unsupported(range, UnsupportedConstruct::LabeledJump));
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
        if let Some(finalizer) = &try_statement.finalizer
            && !finalizer.data().statements.is_empty()
        {
            // A non-empty `finally` must run on normal, caught, and abrupt
            // (return/throw/break) completions; the exception-handler model
            // only routes uncaught throws to a handler, so faithful `finally`
            // is not expressible.
            return Err(self.unsupported(range, UnsupportedConstruct::TryFinally));
        }
        let Some(handler_clause) = &try_statement.handler else {
            // `try`/`finally` with an empty/absent finalizer: the try block has
            // no exceptional route to model, so run it inline.
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
            // An empty protected range can never throw; the catch clause is
            // dead and the handler would violate the non-empty-range invariant.
            return Ok(());
        }
        let over_catch = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;

        let catch_register = self.alloc_register(range)?;
        let handler_pc = self.next_pc();
        self.push_scope();
        let clause = handler_clause.data();
        if let Some(binding) = &clause.binding {
            match binding.data() {
                BindingPattern::Identifier(identifier) => {
                    let name = self.identifier_text(identifier)?;
                    self.declare(name, Binding::Local(catch_register), false);
                }
                BindingPattern::Missing(missing) => {
                    self.pop_scope();
                    return Err(self.missing(binding.range(), missing.expected()));
                }
                _ => {
                    self.pop_scope();
                    return Err(self
                        .unsupported(binding.range(), UnsupportedConstruct::DestructuringBinding));
                }
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
            let name = match data.binding.data() {
                BindingPattern::Identifier(identifier) => self.identifier_text(identifier)?,
                BindingPattern::Missing(missing) => {
                    return Err(self.missing(data.binding.range(), missing.expected()));
                }
                _ => {
                    return Err(self.unsupported(
                        data.binding.range(),
                        UnsupportedConstruct::DestructuringBinding,
                    ));
                }
            };
            let value = match &data.initializer {
                Some(initializer) => self.lower_expression(builder, initializer)?,
                None => self.load_constant(builder, Constant::Undefined, range)?,
            };
            let home = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::Move {
                    dst: home,
                    src: value,
                },
            )?;
            self.declare(name, Binding::Local(home), function_scoped);
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
            Some(identifier) => Some(self.identifier_text(identifier)?),
            None => None,
        };
        // Reuse the id hoisting reserved for this name; only an anonymous
        // (unhoistable) declaration needs a fresh reservation.
        let id = match name.as_deref().and_then(|name| self.resolve(name)) {
            Some(Binding::Function(id)) => id,
            _ => builder.reserve_function(range)?,
        };
        self.build_function(builder, id, range, name, function)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    /// Lowers a value expression and returns the register holding its value.
    fn lower_expression(
        &mut self,
        builder: &mut ModuleBuilder,
        expression: &Expr,
    ) -> Result<Register, LowerError> {
        let range = expression.range();
        match expression.data() {
            Expression::Identifier(identifier) => {
                let name = self.identifier_text(identifier)?;
                self.read_binding(builder, &name, range)
            }
            Expression::This => Err(self.unsupported(range, UnsupportedConstruct::ThisExpression)),
            Expression::Super => {
                Err(self.unsupported(range, UnsupportedConstruct::SuperExpression))
            }
            Expression::Literal(literal) => self.lower_literal(builder, range, literal),
            Expression::Template(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::TemplateLiteral))
            }
            Expression::TaggedTemplate(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::TaggedTemplate))
            }
            Expression::Array(array) => {
                if array.elements.is_empty() {
                    let dst = self.alloc_register(range)?;
                    self.emit(range, Instruction::CreateArray { dst })?;
                    Ok(dst)
                } else {
                    Err(self.unsupported(range, UnsupportedConstruct::ArrayElements))
                }
            }
            Expression::Object(object) => self.lower_object(builder, range, object),
            Expression::Function(function) => {
                let id = builder.reserve_function(range)?;
                let name = match &function.function.name {
                    Some(identifier) => Some(self.identifier_text(identifier)?),
                    None => None,
                };
                self.build_function(builder, id, range, name, &function.function)?;
                self.materialize_function(range, id)
            }
            Expression::Class(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::ClassExpression))
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
            Expression::Yield(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::YieldExpression))
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
                last.ok_or_else(|| self.missing(range, NodeKind::SequenceExpression))
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
            Expression::Import(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::DynamicImport))
            }
            Expression::Meta(_) => Err(self.unsupported(range, UnsupportedConstruct::MetaProperty)),
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
                return self.load_constant(builder, Constant::Undefined, range);
            }
            UnaryOperator::Delete => return self.lower_delete(builder, range, &unary.argument),
            UnaryOperator::Plus => UnaryOp::Plus,
            UnaryOperator::Minus => UnaryOp::Negate,
            UnaryOperator::Not => UnaryOp::LogicalNot,
            UnaryOperator::BitNot => UnaryOp::BitwiseNot,
            UnaryOperator::Typeof => UnaryOp::TypeOf,
        };
        let operand = self.lower_expression(builder, &unary.argument)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::Unary { dst, op, operand })?;
        Ok(dst)
    }

    fn lower_delete(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        argument: &Expr,
    ) -> Result<Register, LowerError> {
        match argument.data() {
            Expression::Member(member) if !member.optional => {
                let object = self.lower_expression(builder, &member.object)?;
                let key = self.member_key(builder, &member.property)?;
                let dst = self.alloc_register(range)?;
                self.emit(range, Instruction::DeleteProperty { dst, object, key })?;
                Ok(dst)
            }
            Expression::Member(_) => {
                Err(self.unsupported(range, UnsupportedConstruct::OptionalChaining))
            }
            Expression::Parenthesized(inner) => self.lower_delete(builder, range, inner),
            _ => Err(self.unsupported(range, UnsupportedConstruct::DeleteBinding)),
        }
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
        // Result lives in one register written on every path before the merge.
        let result = self.alloc_register(range)?;
        let left = self.lower_expression(builder, &logical.left)?;
        self.emit(
            range,
            Instruction::Move {
                dst: result,
                src: left,
            },
        )?;

        let short_circuit = match logical.operator {
            LogicalOperator::And => {
                // `a && b`: keep `a` when it is falsy, else evaluate `b`.
                self.emit(
                    range,
                    Instruction::JumpIfFalse {
                        condition: left,
                        target: Pc::new(0),
                    },
                )?
            }
            LogicalOperator::Or => {
                // `a || b`: keep `a` when it is truthy, else evaluate `b`.
                self.emit(
                    range,
                    Instruction::JumpIfTrue {
                        condition: left,
                        target: Pc::new(0),
                    },
                )?
            }
            LogicalOperator::Nullish => {
                // `a ?? b`: keep `a` unless it is `null` or `undefined`.
                let is_nullish = self.compute_nullish(builder, range, left)?;
                self.emit(
                    range,
                    Instruction::JumpIfFalse {
                        condition: is_nullish,
                        target: Pc::new(0),
                    },
                )?
            }
        };
        let right = self.lower_expression(builder, &logical.right)?;
        self.emit(
            range,
            Instruction::Move {
                dst: result,
                src: right,
            },
        )?;
        let end = self.next_pc();
        self.patch_jump(short_circuit, end);
        Ok(result)
    }

    /// Computes whether `value` is `null` or `undefined` into a fresh register.
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
        // Loose `== null` is true for both `null` and `undefined`, exactly the
        // nullish set, so a single loose comparison suffices.
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
        self.emit(
            range,
            Instruction::Move {
                dst: result,
                src: consequent,
            },
        )?;
        let to_end = self.emit(range, Instruction::Jump { target: Pc::new(0) })?;
        let alternate_pc = self.next_pc();
        self.patch_jump(to_alternate, alternate_pc);
        let alternate = self.lower_expression(builder, &conditional.alternate)?;
        self.emit(
            range,
            Instruction::Move {
                dst: result,
                src: alternate,
            },
        )?;
        let end = self.next_pc();
        self.patch_jump(to_end, end);
        Ok(result)
    }

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
            AssignmentTarget::Object(_) | AssignmentTarget::Array(_) => Err(self.unsupported(
                assignment.left.range(),
                UnsupportedConstruct::DestructuringAssignment,
            )),
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
        let target_range = assignment.left.range();
        match compound_operator(assignment.operator) {
            None => {
                let home = self.resolve_assignable(builder, name, target_range)?;
                let value = self.lower_expression(builder, &assignment.right)?;
                self.emit(
                    range,
                    Instruction::Move {
                        dst: home,
                        src: value,
                    },
                )?;
                Ok(home)
            }
            Some(CompoundOp::Arithmetic(op)) => {
                let home = self.resolve_assignable(builder, name, target_range)?;
                let right = self.lower_expression(builder, &assignment.right)?;
                let result = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Binary {
                        dst: result,
                        op,
                        left: home,
                        right,
                    },
                )?;
                self.emit(
                    range,
                    Instruction::Move {
                        dst: home,
                        src: result,
                    },
                )?;
                Ok(home)
            }
            Some(CompoundOp::Logical(op)) => {
                self.lower_logical_assignment(builder, range, name, op, assignment)
            }
        }
    }

    /// Lowers `a &&= b`, `a ||= b`, `a ??= b` to a conditional store into the
    /// existing home register, preserving short-circuit semantics.
    fn lower_logical_assignment(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        name: &str,
        op: LogicalOperator,
        assignment: &AssignmentExpression,
    ) -> Result<Register, LowerError> {
        let home = self.resolve_assignable(builder, name, assignment.left.range())?;
        let skip = match op {
            LogicalOperator::And => self.emit(
                range,
                Instruction::JumpIfFalse {
                    condition: home,
                    target: Pc::new(0),
                },
            )?,
            LogicalOperator::Or => self.emit(
                range,
                Instruction::JumpIfTrue {
                    condition: home,
                    target: Pc::new(0),
                },
            )?,
            LogicalOperator::Nullish => {
                let is_nullish = self.compute_nullish(builder, range, home)?;
                self.emit(
                    range,
                    Instruction::JumpIfFalse {
                        condition: is_nullish,
                        target: Pc::new(0),
                    },
                )?
            }
        };
        let value = self.lower_expression(builder, &assignment.right)?;
        self.emit(
            range,
            Instruction::Move {
                dst: home,
                src: value,
            },
        )?;
        let end = self.next_pc();
        self.patch_jump(skip, end);
        Ok(home)
    }

    fn lower_member_assignment(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &crate::syntax::AssignmentMemberTarget,
        assignment: &AssignmentExpression,
    ) -> Result<Register, LowerError> {
        let object = self.lower_expression(builder, &member.object)?;
        let key = self.member_property_key(builder, &member.property)?;
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
            Some(CompoundOp::Logical(_)) => {
                // Logical member assignment requires re-reading the property in
                // a short-circuit branch; expressible but out of scope here.
                Err(self.unsupported(range, UnsupportedConstruct::OptionalChaining))
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
                let home = self.resolve_assignable(builder, &name, update.argument.range())?;
                let old = self.alloc_register(range)?;
                self.emit(
                    range,
                    Instruction::Move {
                        dst: old,
                        src: home,
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
                    Instruction::Move {
                        dst: home,
                        src: updated,
                    },
                )?;
                Ok(if update.prefix { updated } else { old })
            }
            AssignmentTarget::Member(member) => {
                let object = self.lower_expression(builder, &member.object)?;
                let key = self.member_property_key(builder, &member.property)?;
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
            AssignmentTarget::Object(_) | AssignmentTarget::Array(_) => Err(self.unsupported(
                update.argument.range(),
                UnsupportedConstruct::DestructuringAssignment,
            )),
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
        let dst = self.alloc_register(range)?;
        let resume = Pc::new(self.code.len() as u32 + 1);
        self.emit(range, Instruction::Suspend { dst, src, resume })?;
        Ok(dst)
    }

    fn lower_member(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        member: &MemberExpression,
    ) -> Result<(Register, Register), LowerError> {
        if member.optional {
            return Err(self.unsupported(range, UnsupportedConstruct::OptionalChaining));
        }
        let object = self.lower_expression(builder, &member.object)?;
        let key = self.member_key(builder, &member.property)?;
        let dst = self.alloc_register(range)?;
        self.emit(range, Instruction::GetProperty { dst, object, key })?;
        Ok((object, dst))
    }

    fn lower_call(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        call: &CallExpression,
    ) -> Result<Register, LowerError> {
        if call.optional {
            return Err(self.unsupported(range, UnsupportedConstruct::OptionalChaining));
        }
        let (callee, this_value) = match call.callee.data() {
            Expression::Member(member) if !member.optional => {
                let (object, value) = self.lower_member(builder, call.callee.range(), member)?;
                (value, object)
            }
            Expression::Member(_) => {
                return Err(self.unsupported(range, UnsupportedConstruct::OptionalChaining));
            }
            _ => {
                let callee = self.lower_expression(builder, &call.callee)?;
                let this_value = self.load_constant(builder, Constant::Undefined, range)?;
                (callee, this_value)
            }
        };
        let temps = self.lower_argument_temps(builder, &call.arguments)?;
        let (args_start, arg_count) = self.build_argument_window(range, &temps, callee)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Call {
                dst,
                callee,
                this_value,
                args_start,
                arg_count,
            },
        )?;
        Ok(dst)
    }

    fn lower_new(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        new: &NewExpression,
    ) -> Result<Register, LowerError> {
        let callee = self.lower_expression(builder, &new.callee)?;
        let temps = self.lower_argument_temps(builder, &new.arguments)?;
        let (args_start, arg_count) = self.build_argument_window(range, &temps, callee)?;
        let dst = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Construct {
                dst,
                callee,
                args_start,
                arg_count,
            },
        )?;
        Ok(dst)
    }

    /// Evaluates each positional argument into its own register.
    fn lower_argument_temps(
        &mut self,
        builder: &mut ModuleBuilder,
        arguments: &[CallArgument],
    ) -> Result<Vec<Register>, LowerError> {
        let mut temps = Vec::with_capacity(arguments.len());
        for argument in arguments {
            match argument {
                CallArgument::Expression(expression) => {
                    temps.push(self.lower_expression(builder, expression)?);
                }
                CallArgument::Spread(spread) => {
                    return Err(self.unsupported(
                        spread.argument.range(),
                        UnsupportedConstruct::SpreadElement,
                    ));
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
        Ok(temps)
    }

    /// Copies argument temporaries into a fresh contiguous register window, as
    /// [`Instruction::Call`]/[`Instruction::Construct`] require.
    fn build_argument_window(
        &mut self,
        range: TextRange,
        temps: &[Register],
        fallback: Register,
    ) -> Result<(Register, u32), LowerError> {
        if temps.is_empty() {
            return Ok((fallback, 0));
        }
        if temps.len() > MAX_REGISTERS as usize {
            return Err(self.error(
                range,
                LowerErrorKind::Capacity(CapacityLimit::ArgumentCount),
            ));
        }
        let start = self.alloc_register(range)?;
        self.emit(
            range,
            Instruction::Move {
                dst: start,
                src: temps[0],
            },
        )?;
        for temp in &temps[1..] {
            let slot = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::Move {
                    dst: slot,
                    src: *temp,
                },
            )?;
        }
        Ok((start, temps.len() as u32))
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
                    if property.modifier != crate::syntax::PropertyModifier::None {
                        return Err(self
                            .unsupported(member.range(), UnsupportedConstruct::AccessorProperty));
                    }
                    let key = self.property_name_key(builder, &property.name)?;
                    let value = self.lower_expression(builder, &property.value)?;
                    self.emit(
                        member.range(),
                        Instruction::SetProperty {
                            object: dst,
                            key,
                            value,
                        },
                    )?;
                }
                ObjectMember::Method(method) => {
                    if method.modifier != crate::syntax::PropertyModifier::None {
                        return Err(self
                            .unsupported(member.range(), UnsupportedConstruct::AccessorProperty));
                    }
                    let key = self.property_name_key(builder, &method.name)?;
                    let id = builder.reserve_function(member.range())?;
                    self.build_function(builder, id, member.range(), None, &method.function)?;
                    let value = self.materialize_function(member.range(), id)?;
                    self.emit(
                        member.range(),
                        Instruction::SetProperty {
                            object: dst,
                            key,
                            value,
                        },
                    )?;
                }
                ObjectMember::Spread(_) => {
                    return Err(
                        self.unsupported(member.range(), UnsupportedConstruct::ObjectSpread)
                    );
                }
                ObjectMember::Missing(missing) => {
                    return Err(self.missing(member.range(), missing.expected()));
                }
            }
        }
        Ok(dst)
    }

    // ------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------

    fn lower_arrow(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        arrow: &ArrowFunction,
    ) -> Result<Register, LowerError> {
        let id = builder.reserve_function(range)?;
        let outer = self.visible_names();
        let mut inner = FunctionContext::new(self.file, outer, false);
        inner.bind_parameters(&arrow.parameters)?;
        match &arrow.body {
            FunctionBody::Block(block) => {
                inner.hoist_functions(builder, &block.data().statements)?;
                for statement in &block.data().statements {
                    inner.lower_statement(builder, statement)?;
                }
                inner.emit_return_undefined(builder, range)?;
            }
            FunctionBody::Expression(expression) => {
                let value = inner.lower_expression(builder, expression)?;
                inner.emit(range, Instruction::Return { value })?;
            }
            FunctionBody::Missing(missing) => {
                return Err(self.missing(range, missing.expected()));
            }
        }
        let flags = FunctionFlags {
            is_async: arrow.is_async,
            is_generator: false,
        };
        let function = inner.into_function(None, flags);
        builder.fill_function(id, function);
        self.materialize_function(range, id)
    }

    /// Lowers a function-like body into a reserved module function slot.
    fn build_function(
        &mut self,
        builder: &mut ModuleBuilder,
        id: FunctionId,
        range: TextRange,
        name: Option<String>,
        function: &FunctionLike,
    ) -> Result<(), LowerError> {
        if let Some(decorator) = function.decorators.first() {
            return Err(self.unsupported(
                decorator.range(),
                UnsupportedConstruct::DecoratedDeclaration,
            ));
        }
        if function.is_generator {
            return Err(self.unsupported(range, UnsupportedConstruct::GeneratorFunction));
        }
        let body = function
            .body
            .as_ref()
            .expect("build_function is only called for functions with a body");
        let block = match body {
            FunctionBody::Block(block) => block,
            FunctionBody::Expression(_) => {
                // An expression-bodied `function` is not valid JS/TS syntax and
                // only arises from recovery; treat it as missing.
                return Err(self.missing(range, NodeKind::BlockStatement));
            }
            FunctionBody::Missing(missing) => {
                return Err(self.missing(range, missing.expected()));
            }
        };

        let outer = self.visible_names();
        let mut inner = FunctionContext::new(self.file, outer, false);
        inner.bind_parameters(&function.parameters)?;
        // A named function expression/declaration can refer to itself.
        if let Some(name) = &name {
            inner.declare(name.clone(), Binding::Function(id), true);
        }
        inner.hoist_functions(builder, &block.data().statements)?;
        for statement in &block.data().statements {
            inner.lower_statement(builder, statement)?;
        }
        inner.emit_return_undefined(builder, range)?;

        let name_constant = match name {
            Some(name) => Some(builder.intern(Constant::String(name), range)?),
            None => None,
        };
        let flags = FunctionFlags {
            is_async: function.is_async,
            is_generator: false,
        };
        let assembled = inner.into_function(name_constant, flags);
        builder.fill_function(id, assembled);
        Ok(())
    }

    /// Allocates the leading parameter registers (`0..parameter_count`), which
    /// the verifier treats as initialized on function entry.
    fn bind_parameters(&mut self, parameters: &[ParameterNode]) -> Result<(), LowerError> {
        for parameter in parameters {
            let data = parameter.data();
            let name = self.simple_parameter_name(parameter, data)?;
            let register = self.alloc_register(parameter.range())?;
            self.parameter_count += 1;
            self.declare(name, Binding::Local(register), true);
        }
        Ok(())
    }

    fn simple_parameter_name(
        &self,
        parameter: &ParameterNode,
        data: &Parameter,
    ) -> Result<String, LowerError> {
        if data.initializer.is_some() {
            return Err(self.unsupported(parameter.range(), UnsupportedConstruct::ComplexParameter));
        }
        match data.binding.data() {
            BindingPattern::Identifier(identifier) => self.identifier_text(identifier),
            BindingPattern::Missing(missing) => {
                Err(self.missing(data.binding.range(), missing.expected()))
            }
            _ => Err(self.unsupported(parameter.range(), UnsupportedConstruct::ComplexParameter)),
        }
    }

    fn emit_return_undefined(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
    ) -> Result<(), LowerError> {
        let value = self.load_constant(builder, Constant::Undefined, range)?;
        self.emit(range, Instruction::Return { value })?;
        Ok(())
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
        if import.type_only {
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
            // `import "m";` evaluates the module for its side effects only.
            return Ok(());
        };
        if let Some(default) = &clause.default {
            let name = self.identifier_text(default)?;
            let key = builder.intern(Constant::String("default".to_owned()), range)?;
            let value = self.alloc_register(range)?;
            self.emit(
                range,
                Instruction::GetProperty {
                    dst: value,
                    object: module,
                    key,
                },
            )?;
            self.declare(name, Binding::Local(value), true);
        }
        match &clause.binding {
            Some(ImportBinding::Namespace(identifier)) => {
                let name = self.identifier_text(identifier)?;
                self.declare(name, Binding::Local(module), true);
            }
            Some(ImportBinding::Named(specifiers)) => {
                for specifier in specifiers {
                    let data = specifier.data();
                    if matches!(data.mode, ImportSpecifierMode::TypeOnly) {
                        continue;
                    }
                    let local = self.identifier_text(&data.local)?;
                    let imported = self.module_export_name(&data.imported)?;
                    let key = builder.intern(Constant::String(imported), range)?;
                    let value = self.alloc_register(range)?;
                    self.emit(
                        range,
                        Instruction::GetProperty {
                            dst: value,
                            object: module,
                            key,
                        },
                    )?;
                    self.declare(local, Binding::Local(value), true);
                }
            }
            None => {}
        }
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
                // Export linkage has no local runtime effect; the declaration
                // itself still executes.
                self.lower_statement(builder, statement)
            }
            ExportDeclaration::Named(ExportNamedDeclaration::Specifiers {
                type_only,
                specifiers,
                source,
                ..
            }) => {
                let runtime = !type_only
                    && (source.is_some()
                        || specifiers.iter().any(|specifier| {
                            matches!(specifier.data().mode, ExportSpecifierMode::Value)
                        }));
                if runtime {
                    Err(self.unsupported(range, UnsupportedConstruct::RuntimeExportSpecifiers))
                } else {
                    Ok(())
                }
            }
            ExportDeclaration::All(all) => {
                if all.type_only {
                    Ok(())
                } else {
                    Err(self.unsupported(range, UnsupportedConstruct::RuntimeExportAll))
                }
            }
            ExportDeclaration::Default(default) => match &default.value {
                ExportDefaultValue::Expression(expression) => {
                    self.lower_expression(builder, expression)?;
                    Ok(())
                }
                ExportDefaultValue::Function(function) => {
                    if function.body.is_none() {
                        return Ok(());
                    }
                    let id = builder.reserve_function(range)?;
                    let name = match &function.name {
                        Some(identifier) => Some(self.identifier_text(identifier)?),
                        None => None,
                    };
                    self.build_function(builder, id, range, name, function)?;
                    Ok(())
                }
                ExportDefaultValue::Class(_) => {
                    Err(self.unsupported(range, UnsupportedConstruct::ExportDefaultClass))
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
    // Property keys and literals
    // ------------------------------------------------------------------

    /// Interns the string key of a read/delete member property.
    fn member_key(
        &mut self,
        builder: &mut ModuleBuilder,
        property: &MemberProperty,
    ) -> Result<ConstantId, LowerError> {
        match property {
            MemberProperty::Named(identifier) => {
                let name = self.identifier_text(identifier)?;
                builder.intern(Constant::String(name), identifier.range())
            }
            MemberProperty::Private(private) => {
                Err(self.unsupported(private.range(), UnsupportedConstruct::PrivateField))
            }
            MemberProperty::Computed(expression) => self.static_computed_key(builder, expression),
        }
    }

    /// The string key of an assignment member target.
    fn member_property_key(
        &mut self,
        builder: &mut ModuleBuilder,
        property: &MemberProperty,
    ) -> Result<ConstantId, LowerError> {
        self.member_key(builder, property)
    }

    /// A computed member/property key is expressible only when it is a literal
    /// string or a canonical non-negative integer; a register-valued key has no
    /// instruction.
    fn static_computed_key(
        &mut self,
        builder: &mut ModuleBuilder,
        expression: &Expr,
    ) -> Result<ConstantId, LowerError> {
        let range = expression.range();
        match expression.data() {
            Expression::Literal(Literal::String(string)) => {
                let value = self.string_literal_value(string)?;
                builder.intern(Constant::String(value), range)
            }
            Expression::Literal(Literal::Number(number)) => {
                let key = self.integer_key_text(number)?;
                builder.intern(Constant::String(key), range)
            }
            Expression::Parenthesized(inner) => self.static_computed_key(builder, inner),
            _ => Err(self.unsupported(range, UnsupportedConstruct::ComputedMemberAccess)),
        }
    }

    /// Interns the string key of an object-literal property name.
    fn property_name_key(
        &mut self,
        builder: &mut ModuleBuilder,
        name: &PropertyName,
    ) -> Result<ConstantId, LowerError> {
        match name {
            PropertyName::Identifier(identifier) => {
                let text = self.identifier_text(identifier)?;
                builder.intern(Constant::String(text), identifier.range())
            }
            PropertyName::String(string) => {
                let value = self.string_literal_value(string)?;
                builder.intern(Constant::String(value), string.range())
            }
            PropertyName::Number(number) => {
                let key = self.integer_key_text(number)?;
                builder.intern(Constant::String(key), number.range())
            }
            PropertyName::Computed(expression) => self
                .static_computed_key(builder, expression)
                .map_err(|error| {
                    if matches!(
                        error.kind,
                        LowerErrorKind::Unsupported(UnsupportedConstruct::ComputedMemberAccess)
                    ) {
                        self.unsupported(
                            expression.range(),
                            UnsupportedConstruct::ComputedPropertyKey,
                        )
                    } else {
                        error
                    }
                }),
            PropertyName::Private(private) => {
                Err(self.unsupported(private.range(), UnsupportedConstruct::PrivateField))
            }
            PropertyName::Missing(missing) => Err(self.error(
                zero_range(),
                LowerErrorKind::MissingSyntax {
                    expected: missing.expected(),
                },
            )),
        }
    }

    /// A numeric property key is expressible only as its canonical integer
    /// `ToString`; non-integers would require a runtime coercion this revision
    /// does not model.
    fn integer_key_text(&self, number: &NumericLiteralNode) -> Result<String, LowerError> {
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
        if value.fract() != 0.0
            || !value.is_finite()
            || value < 0.0
            || value > 9_007_199_254_740_991.0
        {
            return Err(self.unsupported(range, UnsupportedConstruct::NonIntegerPropertyKey));
        }
        Ok(format!("{}", value as u64))
    }

    fn lower_literal(
        &mut self,
        builder: &mut ModuleBuilder,
        range: TextRange,
        literal: &Literal,
    ) -> Result<Register, LowerError> {
        match literal {
            Literal::Number(number) => self.lower_numeric_literal(builder, number, false),
            Literal::String(string) => {
                let value = self.string_literal_value(string)?;
                self.load_constant(builder, Constant::String(value), range)
            }
            Literal::Boolean(boolean) => {
                let value = self.boolean_literal_value(boolean)?;
                self.load_constant(builder, Constant::Boolean(value), range)
            }
            Literal::Null(_) => self.load_constant(builder, Constant::Null, range),
            Literal::BigInt(_) => self.lower_bigint_literal(builder, range, literal),
            Literal::Regex(_) => Err(self.unsupported(range, UnsupportedConstruct::RegexLiteral)),
        }
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
        negate: bool,
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
        let value = if negate { -value } else { value };
        self.load_constant(builder, number_constant(value), range)
    }

    /// An escape-free literal's spelling is exactly its value; escapes would
    /// require a cooked-value pipeline this revision does not define, so they
    /// are a typed error instead of a silently wrong constant.
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
        if interior.contains('\\') {
            return Err(self.unsupported(range, UnsupportedConstruct::EscapedStringLiteral));
        }
        Ok(interior.to_owned())
    }

    fn boolean_literal_value(&self, boolean: &BooleanLiteralNode) -> Result<bool, LowerError> {
        let token = boolean.data().token();
        match token.kind() {
            TokenKind::KwTrue if !token.is_missing() => Ok(true),
            TokenKind::KwFalse if !token.is_missing() => Ok(false),
            _ => Err(self.missing(boolean.range(), NodeKind::BooleanLiteral)),
        }
    }
}

/// The `for` head range used for the back-edge jump's diagnostic anchor.
fn head_range(for_statement: &ForStatement) -> TextRange {
    for_statement
        .test
        .as_ref()
        .map_or_else(|| for_statement.body.range(), |test| test.range())
}

/// Maps a source binary operator to its bytecode counterpart. Every JavaScript
/// binary operator has an exact instruction-set operator.
fn map_binary_operator(operator: BinaryOperator) -> BinaryOp {
    match operator {
        BinaryOperator::Add => BinaryOp::Add,
        BinaryOperator::Subtract => BinaryOp::Subtract,
        BinaryOperator::Multiply => BinaryOp::Multiply,
        BinaryOperator::Divide => BinaryOp::Divide,
        BinaryOperator::Remainder => BinaryOp::Remainder,
        BinaryOperator::Exponentiate => BinaryOp::Exponent,
        BinaryOperator::LeftShift => BinaryOp::ShiftLeft,
        BinaryOperator::SignedRightShift => BinaryOp::ShiftRight,
        BinaryOperator::UnsignedRightShift => BinaryOp::UnsignedShiftRight,
        BinaryOperator::LessThan => BinaryOp::LessThan,
        BinaryOperator::LessThanOrEqual => BinaryOp::LessThanOrEqual,
        BinaryOperator::GreaterThan => BinaryOp::GreaterThan,
        BinaryOperator::GreaterThanOrEqual => BinaryOp::GreaterThanOrEqual,
        BinaryOperator::In => BinaryOp::In,
        BinaryOperator::Instanceof => BinaryOp::InstanceOf,
        BinaryOperator::Equal => BinaryOp::Equal,
        BinaryOperator::NotEqual => BinaryOp::NotEqual,
        BinaryOperator::StrictEqual => BinaryOp::StrictEqual,
        BinaryOperator::StrictNotEqual => BinaryOp::StrictNotEqual,
        BinaryOperator::BitAnd => BinaryOp::BitAnd,
        BinaryOperator::BitXor => BinaryOp::BitXor,
        BinaryOperator::BitOr => BinaryOp::BitOr,
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
        AssignmentOperator::BitXorAssign => BinaryOp::BitXor,
        AssignmentOperator::BitOrAssign => BinaryOp::BitOr,
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

/// Canonicalizes a decimal bigint lexeme (`123n`, `1_000n`) to the canonical
/// decimal text [`BigIntLiteral`] accepts, or `None` for non-decimal forms.
fn canonical_bigint_text(lexeme: &str) -> Option<String> {
    let digits = lexeme.strip_suffix('n')?;
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() || !cleaned.bytes().all(|b| b.is_ascii_digit()) {
        // Empty, or a `0x`/`0o`/`0b` radix prefix: not a decimal integer.
        return None;
    }
    let trimmed = cleaned.trim_start_matches('0');
    Some(if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// Cooks a scanned numeric lexeme into its ECMAScript number value.
///
/// Numeric separators are removed; `0x`/`0o`/`0b` forms use exact integer
/// parsing (with a deterministic digit fold once past `u128`), and decimal
/// forms including fractions and exponents use IEEE-754 parsing.
fn cook_number(lexeme: &str) -> Option<f64> {
    let cleaned: String = lexeme.chars().filter(|c| *c != '_').collect();
    let bytes = cleaned.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let digits = &cleaned[2..];
        match bytes[1] {
            b'x' | b'X' => return radix_value(digits, 16),
            b'o' | b'O' => return radix_value(digits, 8),
            b'b' | b'B' => return radix_value(digits, 2),
            _ => {}
        }
    }
    cleaned.parse::<f64>().ok().filter(|value| !value.is_nan())
}

fn radix_value(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    match u128::from_str_radix(digits, radix) {
        Ok(value) => Some(value as f64),
        Err(_) => {
            let mut value = 0f64;
            for digit in digits.chars() {
                value = value * f64::from(radix) + f64::from(digit.to_digit(radix)?);
            }
            Some(value)
        }
    }
}

/// Chooses the canonical pool representation for one number value: exact
/// non-negative-zero integers in `i32` range intern as `Int32`, everything else
/// as canonical IEEE-754 bits.
fn number_constant(value: f64) -> Constant {
    let truncated = value as i32;
    if f64::from(truncated) == value && !(value == 0.0 && value.is_sign_negative()) {
        Constant::Int32(truncated)
    } else {
        Constant::Number(NumberBits::from_f64(value))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bamts_bytecode::{
        BinaryOp, Constant, DecodeLimits, Instruction, Module, UnaryOp, decode_verified,
    };

    use super::{
        CapacityLimit, LowerError, LowerErrorKind, LowerOptions, UnsupportedConstruct, lower,
    };
    use crate::source::{ScriptKind, SourceId, SourceText, TextRange, Utf16Pos};
    use crate::syntax::{
        ArrayLiteral, ArrowFunction, AssignmentExpression, AssignmentMemberTarget,
        AssignmentOperator, AssignmentTarget, AwaitExpression, BinaryExpression, BinaryOperator,
        BindingPattern, Block, BooleanLiteral, BooleanLiteralNode, CallArgument, CallExpression,
        ConditionalExpression, DoWhileStatement, Expr, Expression, ExpressionStatement,
        ForInitializer, ForStatement, FunctionBody, FunctionDeclaration, FunctionLike, Identifier,
        IdentifierNode, IfStatement, Literal, LogicalExpression, LogicalOperator, MemberExpression,
        MemberProperty, NewExpression, Node, NodeId, NullLiteral, NumericLiteral, ObjectLiteral,
        ObjectMember, ObjectProperty, Parameter, ParameterModifiers, ParameterNode,
        PropertyModifier, PropertyName, ReturnStatement, SourceFile, Statement, Stmt,
        StringLiteral, StringLiteralNode, SwitchCase, SwitchStatement, ThrowStatement, Token,
        TokenKind, UnaryExpression, UnaryOperator, UpdateExpression, UpdateOperator,
        VariableDeclaration, VariableDeclarator, VariableKind, WhileStatement,
    };

    /// Builds ASCII test sources token by token so lexeme lookups through
    /// [`SourceFile::token_text`] observe real text.
    struct AstBuilder {
        text: String,
        next_id: u32,
    }

    impl AstBuilder {
        fn new() -> Self {
            Self {
                text: String::new(),
                next_id: 0,
            }
        }

        fn id(&mut self) -> NodeId {
            let id = NodeId::new(self.next_id);
            self.next_id += 1;
            id
        }

        fn token(&mut self, kind: TokenKind, lexeme: &str) -> Token {
            let start = self.text.len();
            self.text.push_str(lexeme);
            let range = range(start, start + lexeme.len());
            self.text.push(' ');
            Token::new(kind, range)
        }

        fn ident(&mut self, name: &str) -> IdentifierNode {
            let token = self.token(TokenKind::Identifier, name);
            Node::new(self.id(), token.range(), Identifier::new(token))
        }

        fn number(&mut self, lexeme: &str) -> Expr {
            let token = self.token(TokenKind::NumericLiteral, lexeme);
            let node = Node::new(self.id(), token.range(), NumericLiteral::new(token));
            self.expr(token.range(), Expression::Literal(Literal::Number(node)))
        }

        fn string(&mut self, contents: &str) -> Expr {
            let node = self.string_node(contents);
            let range = node.range();
            self.expr(range, Expression::Literal(Literal::String(node)))
        }

        fn string_node(&mut self, contents: &str) -> StringLiteralNode {
            let lexeme = format!("\"{contents}\"");
            let token = self.token(TokenKind::StringLiteral, &lexeme);
            Node::new(self.id(), token.range(), StringLiteral::new(token))
        }

        fn boolean(&mut self, value: bool) -> Expr {
            let (kind, lexeme) = if value {
                (TokenKind::KwTrue, "true")
            } else {
                (TokenKind::KwFalse, "false")
            };
            let token = self.token(kind, lexeme);
            let node: BooleanLiteralNode =
                Node::new(self.id(), token.range(), BooleanLiteral::new(token));
            self.expr(token.range(), Expression::Literal(Literal::Boolean(node)))
        }

        fn null(&mut self) -> Expr {
            let token = self.token(TokenKind::KwNull, "null");
            let node = Node::new(self.id(), token.range(), NullLiteral::new(token));
            self.expr(token.range(), Expression::Literal(Literal::Null(node)))
        }

        fn name_expr(&mut self, name: &str) -> Expr {
            let identifier = self.ident(name);
            let range = identifier.range();
            self.expr(range, Expression::Identifier(identifier))
        }

        fn expr(&mut self, range: TextRange, data: Expression) -> Expr {
            Node::new(self.id(), range, data)
        }

        fn stmt(&mut self, range: TextRange, data: Statement) -> Stmt {
            Node::new(self.id(), range, data)
        }

        fn expr_stmt(&mut self, expression: Expr) -> Stmt {
            let range = expression.range();
            self.stmt(
                range,
                Statement::Expression(ExpressionStatement {
                    expression: Box::new(expression),
                }),
            )
        }

        fn binary(&mut self, operator: BinaryOperator, left: Expr, right: Expr) -> Expr {
            let range = span(left.range(), right.range());
            self.expr(
                range,
                Expression::Binary(BinaryExpression {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                }),
            )
        }

        fn add(&mut self, left: Expr, right: Expr) -> Expr {
            self.binary(BinaryOperator::Add, left, right)
        }

        fn var_stmt(&mut self, kind: VariableKind, name: &str, initializer: Option<Expr>) -> Stmt {
            let identifier = self.ident(name);
            let range = identifier.range();
            let binding = Node::new(self.id(), range, BindingPattern::Identifier(identifier));
            let declarator = Node::new(
                self.id(),
                range,
                VariableDeclarator {
                    binding,
                    definite: false,
                    type_annotation: None,
                    initializer: initializer.map(Box::new),
                },
            );
            self.stmt(
                range,
                Statement::Variable(VariableDeclaration {
                    kind,
                    declarations: vec![declarator],
                }),
            )
        }

        fn assign_expr(&mut self, name: &str, operator: AssignmentOperator, value: Expr) -> Expr {
            let identifier = self.ident(name);
            let target_range = identifier.range();
            let target = Node::new(
                self.id(),
                target_range,
                AssignmentTarget::Identifier(identifier),
            );
            let range = span(target_range, value.range());
            self.expr(
                range,
                Expression::Assignment(AssignmentExpression {
                    operator,
                    left: target,
                    right: Box::new(value),
                }),
            )
        }

        fn assign_stmt(&mut self, name: &str, operator: AssignmentOperator, value: Expr) -> Stmt {
            let assignment = self.assign_expr(name, operator, value);
            self.expr_stmt(assignment)
        }

        fn member(&mut self, object: Expr, property: &str) -> Expr {
            let identifier = self.ident(property);
            let range = span(object.range(), identifier.range());
            self.expr(
                range,
                Expression::Member(MemberExpression {
                    object: Box::new(object),
                    property: MemberProperty::Named(identifier),
                    optional: false,
                }),
            )
        }

        fn call(&mut self, callee: Expr, arguments: Vec<Expr>) -> Expr {
            let range = callee.range();
            let arguments = arguments
                .into_iter()
                .map(|argument| CallArgument::Expression(Box::new(argument)))
                .collect();
            self.expr(
                range,
                Expression::Call(CallExpression {
                    callee: Box::new(callee),
                    optional: false,
                    type_arguments: None,
                    arguments,
                }),
            )
        }

        fn return_stmt(&mut self, argument: Option<Expr>) -> Stmt {
            let range = argument
                .as_ref()
                .map_or_else(|| range(self.text.len(), self.text.len()), Expr::range);
            self.stmt(
                range,
                Statement::Return(ReturnStatement {
                    argument: argument.map(Box::new),
                }),
            )
        }

        fn param(&mut self, name: &str) -> ParameterNode {
            let identifier = self.ident(name);
            let range = identifier.range();
            let binding = Node::new(self.id(), range, BindingPattern::Identifier(identifier));
            Node::new(
                self.id(),
                range,
                Parameter {
                    decorators: Vec::new(),
                    modifiers: ParameterModifiers::default(),
                    binding,
                    optional: false,
                    type_annotation: None,
                    initializer: None,
                },
            )
        }

        fn function_stmt(
            &mut self,
            name: &str,
            params: Vec<ParameterNode>,
            statements: Vec<Stmt>,
        ) -> Stmt {
            let identifier = self.ident(name);
            let range = identifier.range();
            let block = Node::new(self.id(), range, Block { statements });
            self.stmt(
                range,
                Statement::Function(FunctionDeclaration {
                    function: FunctionLike {
                        decorators: Vec::new(),
                        name: Some(identifier),
                        is_async: false,
                        is_generator: false,
                        type_parameters: None,
                        parameters: params,
                        return_type: None,
                        body: Some(FunctionBody::Block(block)),
                    },
                }),
            )
        }

        fn block_stmt(&mut self, statements: Vec<Stmt>) -> Stmt {
            let block = Node::new(self.id(), range(0, 0), Block { statements });
            let range = block.range();
            self.stmt(range, Statement::Block(block))
        }

        fn finish(mut self, script_kind: ScriptKind, statements: Vec<Stmt>) -> SourceFile {
            let end = self.text.len();
            let eof = Token::new(TokenKind::EndOfFile, range(end, end));
            let id = self.id();
            SourceFile::new(
                id,
                SourceId::from(7u32),
                script_kind,
                range(0, end.max(1)),
                Arc::new(SourceText::new(self.text)),
                Vec::new(),
                statements,
                eof,
                Vec::new(),
            )
        }
    }

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end.max(start)))
            .expect("test ranges are ordered")
    }

    fn span(left: TextRange, right: TextRange) -> TextRange {
        TextRange::new(left.start(), right.end()).expect("test spans are ordered")
    }

    fn ts_options() -> LowerOptions {
        LowerOptions {
            javascript_compatibility: false,
        }
    }

    fn js_options() -> LowerOptions {
        LowerOptions {
            javascript_compatibility: true,
        }
    }

    fn error_kind(result: Result<impl std::fmt::Debug, LowerError>) -> LowerErrorKind {
        result.expect_err("lowering must reject this input").kind
    }

    /// Every lowered module already passed `Module::verify`; re-encoding and
    /// decode-verifying it proves the wire form round-trips too.
    fn assert_round_trips(module: &Module<bamts_bytecode::Verified>) {
        let bytes = module.encode();
        decode_verified(&bytes, &DecodeLimits::default())
            .expect("a verified module re-decodes and re-verifies");
    }

    #[test]
    fn typescript_addition_lowers_to_verified_code() {
        let mut b = AstBuilder::new();
        let one = b.number("1");
        let two = b.number("2");
        let a_stmt = b.var_stmt(VariableKind::Let, "a", Some(one));
        let b_stmt = b.var_stmt(VariableKind::Let, "b", Some(two));
        let a_ref = b.name_expr("a");
        let b_ref = b.name_expr("b");
        let sum = b.add(a_ref, b_ref);
        let c_stmt = b.var_stmt(VariableKind::Const, "c", Some(sum));
        let file = b.finish(ScriptKind::TypeScript, vec![a_stmt, b_stmt, c_stmt]);

        let module = lower(&file, ts_options()).expect("supported kernel lowers");
        assert_eq!(
            module.constants(),
            &[Constant::Int32(1), Constant::Int32(2)]
        );
        assert_eq!(module.functions().len(), 1);
        let entry = &module.functions()[module.entry().get() as usize];
        // A Binary(Add) instruction replaces the old dedicated Add opcode.
        assert!(entry.code().iter().any(|instruction| matches!(
            instruction,
            Instruction::Binary {
                op: BinaryOp::Add,
                ..
            }
        )));
        assert!(matches!(entry.code().last(), Some(Instruction::Halt)));
        assert_round_trips(&module);
    }

    #[test]
    fn every_binary_operator_maps_to_an_instruction() {
        let operators = [
            BinaryOperator::Subtract,
            BinaryOperator::Multiply,
            BinaryOperator::Divide,
            BinaryOperator::Remainder,
            BinaryOperator::Exponentiate,
            BinaryOperator::LeftShift,
            BinaryOperator::SignedRightShift,
            BinaryOperator::UnsignedRightShift,
            BinaryOperator::LessThan,
            BinaryOperator::GreaterThanOrEqual,
            BinaryOperator::In,
            BinaryOperator::Instanceof,
            BinaryOperator::StrictEqual,
            BinaryOperator::BitOr,
        ];
        for operator in operators {
            let mut b = AstBuilder::new();
            let left = b.number("3");
            let right = b.number("4");
            let expression = b.binary(operator, left, right);
            let statement = b.expr_stmt(expression);
            let file = b.finish(ScriptKind::TypeScript, vec![statement]);
            let module = lower(&file, ts_options())
                .unwrap_or_else(|error| panic!("{operator:?} lowers: {error}"));
            let entry = &module.functions()[module.entry().get() as usize];
            assert!(
                entry
                    .code()
                    .iter()
                    .any(|instruction| matches!(instruction, Instruction::Binary { .. })),
                "{operator:?} emits a Binary instruction"
            );
            assert_round_trips(&module);
        }
    }

    #[test]
    fn unary_operators_lower_faithfully() {
        for (operator, expected) in [
            (UnaryOperator::Minus, UnaryOp::Negate),
            (UnaryOperator::Not, UnaryOp::LogicalNot),
            (UnaryOperator::BitNot, UnaryOp::BitwiseNot),
            (UnaryOperator::Typeof, UnaryOp::TypeOf),
            (UnaryOperator::Plus, UnaryOp::Plus),
        ] {
            let mut b = AstBuilder::new();
            let operand = b.name_expr("undefined");
            let range = operand.range();
            let unary = b.expr(
                range,
                Expression::Unary(UnaryExpression {
                    operator,
                    argument: Box::new(operand),
                }),
            );
            let statement = b.expr_stmt(unary);
            let file = b.finish(ScriptKind::TypeScript, vec![statement]);
            let module = lower(&file, ts_options()).expect("unary lowers");
            let entry = &module.functions()[module.entry().get() as usize];
            assert!(
                entry.code().iter().any(|instruction| matches!(
                    instruction,
                    Instruction::Unary { op, .. } if *op == expected
                )),
                "{operator:?} lowers to {expected:?}"
            );
        }
    }

    #[test]
    fn conditional_expression_writes_result_on_both_paths() {
        let mut b = AstBuilder::new();
        let test = b.boolean(true);
        let consequent = b.number("1");
        let alternate = b.number("2");
        let range = test.range();
        let conditional = b.expr(
            range,
            Expression::Conditional(ConditionalExpression {
                test: Box::new(test),
                consequent: Box::new(consequent),
                alternate: Box::new(alternate),
            }),
        );
        let statement = b.var_stmt(VariableKind::Const, "x", Some(conditional));
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("conditional lowers and verifies");
        assert_round_trips(&module);
    }

    #[test]
    fn logical_operators_short_circuit_and_verify() {
        for operator in [
            LogicalOperator::And,
            LogicalOperator::Or,
            LogicalOperator::Nullish,
        ] {
            let mut b = AstBuilder::new();
            let left = b.number("1");
            let right = b.number("2");
            let range = left.range();
            let logical = b.expr(
                range,
                Expression::Logical(LogicalExpression {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                }),
            );
            let statement = b.var_stmt(VariableKind::Const, "x", Some(logical));
            let file = b.finish(ScriptKind::TypeScript, vec![statement]);
            let module = lower(&file, ts_options())
                .unwrap_or_else(|error| panic!("{operator:?} lowers: {error}"));
            assert_round_trips(&module);
        }
    }

    #[test]
    fn if_else_lowers_to_branches_and_verifies() {
        let mut b = AstBuilder::new();
        let init = b.number("0");
        let decl = b.var_stmt(VariableKind::Let, "x", Some(init));
        let test = b.name_expr("x");
        let then_value = b.number("1");
        let then_stmt = b.assign_stmt("x", AssignmentOperator::Assign, then_value);
        let else_value = b.number("2");
        let else_stmt = b.assign_stmt("x", AssignmentOperator::Assign, else_value);
        let range = test.range();
        let if_stmt = b.stmt(
            range,
            Statement::If(IfStatement {
                test: Box::new(test),
                consequent: Box::new(then_stmt),
                alternate: Some(Box::new(else_stmt)),
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![decl, if_stmt]);
        let module = lower(&file, js_options()).expect("if/else lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::JumpIfFalse { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::Jump { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn while_loop_with_break_and_continue_verifies() {
        let mut b = AstBuilder::new();
        let init = b.number("0");
        let decl = b.var_stmt(VariableKind::Let, "i", Some(init));
        let test = b.name_expr("i");
        let inc_target = b.ident("i");
        let inc_range = inc_target.range();
        let inc_target = Node::new(b.id(), inc_range, AssignmentTarget::Identifier(inc_target));
        let inc = b.expr(
            inc_range,
            Expression::Update(UpdateExpression {
                operator: UpdateOperator::Increment,
                argument: Box::new(inc_target),
                prefix: false,
            }),
        );
        let inc_stmt = b.expr_stmt(inc);
        let break_stmt = b.stmt(
            range(0, 0),
            Statement::Break(crate::syntax::JumpStatement { label: None }),
        );
        let body = b.block_stmt(vec![inc_stmt, break_stmt]);
        let range = test.range();
        let while_stmt = b.stmt(
            range,
            Statement::While(WhileStatement {
                test: Box::new(test),
                body: Box::new(body),
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![decl, while_stmt]);
        let module = lower(&file, js_options()).expect("while loop lowers");
        assert_round_trips(&module);
    }

    #[test]
    fn c_style_for_loop_verifies() {
        let mut b = AstBuilder::new();
        let init = b.number("0");
        let init_decl = b.var_stmt(VariableKind::Let, "i", Some(init));
        let init_decl = match init_decl.into_data() {
            Statement::Variable(declaration) => declaration,
            _ => unreachable!(),
        };
        let ten = b.number("10");
        let i_ref = b.name_expr("i");
        let test = b.binary(BinaryOperator::LessThan, i_ref, ten);
        let update_target = b.ident("i");
        let update_range = update_target.range();
        let update_target = Node::new(
            b.id(),
            update_range,
            AssignmentTarget::Identifier(update_target),
        );
        let update = b.expr(
            update_range,
            Expression::Update(UpdateExpression {
                operator: UpdateOperator::Increment,
                argument: Box::new(update_target),
                prefix: false,
            }),
        );
        let body = b.block_stmt(Vec::new());
        let range = test.range();
        let for_stmt = b.stmt(
            range,
            Statement::For(ForStatement {
                initializer: Some(ForInitializer::Variable(init_decl)),
                test: Some(Box::new(test)),
                update: Some(Box::new(update)),
                body: Box::new(body),
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![for_stmt]);
        let module = lower(&file, js_options()).expect("for loop lowers");
        assert_round_trips(&module);
    }

    #[test]
    fn do_while_loop_verifies() {
        let mut b = AstBuilder::new();
        let init = b.number("0");
        let decl = b.var_stmt(VariableKind::Let, "i", Some(init));
        let target = b.ident("i");
        let target_range = target.range();
        let target = Node::new(b.id(), target_range, AssignmentTarget::Identifier(target));
        let inc = b.expr(
            target_range,
            Expression::Update(UpdateExpression {
                operator: UpdateOperator::Increment,
                argument: Box::new(target),
                prefix: false,
            }),
        );
        let body = b.expr_stmt(inc);
        let test = b.boolean(false);
        let range = test.range();
        let do_while = b.stmt(
            range,
            Statement::DoWhile(DoWhileStatement {
                body: Box::new(body),
                test: Box::new(test),
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![decl, do_while]);
        let module = lower(&file, js_options()).expect("do/while lowers");
        assert_round_trips(&module);
    }

    #[test]
    fn switch_statement_lowers_and_verifies() {
        let mut b = AstBuilder::new();
        let disc = b.number("1");
        let one = b.number("1");
        let assign_a_value = b.string("a");
        let assign_a = b.assign_stmt("x", AssignmentOperator::Assign, assign_a_value);
        let break_a = b.stmt(
            range(0, 0),
            Statement::Break(crate::syntax::JumpStatement { label: None }),
        );
        let case_one = Node::new(
            b.id(),
            range(0, 0),
            SwitchCase {
                test: Some(Box::new(one)),
                consequent: vec![assign_a, break_a],
            },
        );
        let default_value = b.string("d");
        let assign_default = b.assign_stmt("x", AssignmentOperator::Assign, default_value);
        let default_case = Node::new(
            b.id(),
            range(0, 0),
            SwitchCase {
                test: None,
                consequent: vec![assign_default],
            },
        );
        let x_init = b.string("init");
        let x_decl = b.var_stmt(VariableKind::Let, "x", Some(x_init));
        let range = disc.range();
        let switch = b.stmt(
            range,
            Statement::Switch(SwitchStatement {
                discriminant: Box::new(disc),
                cases: vec![case_one, default_case],
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![x_decl, switch]);
        let module = lower(&file, js_options()).expect("switch lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(entry.code().iter().any(|i| matches!(
            i,
            Instruction::Binary {
                op: BinaryOp::StrictEqual,
                ..
            }
        )));
        assert_round_trips(&module);
    }

    #[test]
    fn try_catch_registers_a_handler() {
        let mut b = AstBuilder::new();
        let thrown = b.string("boom");
        let throw = b.stmt(
            thrown.range(),
            Statement::Throw(ThrowStatement {
                argument: Box::new(thrown),
            }),
        );
        let try_block = Node::new(
            b.id(),
            range(0, 0),
            Block {
                statements: vec![throw],
            },
        );
        let caught = b.ident("e");
        let caught_range = caught.range();
        let caught_binding = Node::new(b.id(), caught_range, BindingPattern::Identifier(caught));
        let e_ref = b.name_expr("e");
        let use_e = b.expr_stmt(e_ref);
        let catch_body = Node::new(
            b.id(),
            range(0, 0),
            Block {
                statements: vec![use_e],
            },
        );
        let handler = Node::new(
            b.id(),
            range(0, 0),
            crate::syntax::CatchClause {
                binding: Some(caught_binding),
                body: catch_body,
            },
        );
        let try_stmt = b.stmt(
            range(0, 0),
            Statement::Try(crate::syntax::TryStatement {
                block: try_block,
                handler: Some(handler),
                finalizer: None,
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![try_stmt]);
        let module = lower(&file, js_options()).expect("try/catch lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert_eq!(entry.handlers().len(), 1);
        assert_round_trips(&module);
    }

    #[test]
    fn non_empty_finally_is_a_typed_error() {
        let mut b = AstBuilder::new();
        let try_block = Node::new(
            b.id(),
            range(0, 0),
            Block {
                statements: Vec::new(),
            },
        );
        let noop = b.number("1");
        let noop_stmt = b.expr_stmt(noop);
        let finalizer = Node::new(
            b.id(),
            range(0, 0),
            Block {
                statements: vec![noop_stmt],
            },
        );
        let try_stmt = b.stmt(
            range(0, 0),
            Statement::Try(crate::syntax::TryStatement {
                block: try_block,
                handler: None,
                finalizer: Some(finalizer),
            }),
        );
        let file = b.finish(ScriptKind::JavaScript, vec![try_stmt]);
        assert_eq!(
            error_kind(lower(&file, js_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::TryFinally)
        );
    }

    #[test]
    fn object_literal_with_static_keys_and_methods_verifies() {
        let mut b = AstBuilder::new();
        let value = b.number("1");
        let prop = ObjectMember::Property(ObjectProperty {
            name: PropertyName::Identifier(b.ident("a")),
            value: Box::new(value),
            modifier: PropertyModifier::None,
            shorthand: false,
        });
        let prop_node = Node::new(b.id(), range(0, 0), prop);
        let method_block = Node::new(
            b.id(),
            range(0, 0),
            Block {
                statements: Vec::new(),
            },
        );
        let method = ObjectMember::Method(crate::syntax::ObjectMethod {
            name: PropertyName::Identifier(b.ident("m")),
            modifier: PropertyModifier::None,
            function: FunctionLike {
                decorators: Vec::new(),
                name: None,
                is_async: false,
                is_generator: false,
                type_parameters: None,
                parameters: Vec::new(),
                return_type: None,
                body: Some(FunctionBody::Block(method_block)),
            },
        });
        let method_node = Node::new(b.id(), range(0, 0), method);
        let object = b.expr(
            range(0, 0),
            Expression::Object(ObjectLiteral {
                members: vec![prop_node, method_node],
            }),
        );
        let statement = b.var_stmt(VariableKind::Const, "o", Some(object));
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("object literal lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::CreateObject { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::SetProperty { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::DefineFunction { .. }))
        );
        assert!(module.functions().len() >= 2);
        assert_round_trips(&module);
    }

    #[test]
    fn member_read_write_and_delete_verify() {
        let mut b = AstBuilder::new();
        let object = b.expr(
            range(0, 0),
            Expression::Object(ObjectLiteral {
                members: Vec::new(),
            }),
        );
        let decl = b.var_stmt(VariableKind::Const, "o", Some(object));
        // o.x = 1
        let o_ref = b.name_expr("o");
        let value = b.number("1");
        let member_target = AssignmentMemberTarget {
            object: Box::new(o_ref),
            property: MemberProperty::Named(b.ident("x")),
        };
        let target = Node::new(b.id(), range(0, 0), AssignmentTarget::Member(member_target));
        let assign = b.expr(
            range(0, 0),
            Expression::Assignment(AssignmentExpression {
                operator: AssignmentOperator::Assign,
                left: target,
                right: Box::new(value),
            }),
        );
        let assign_stmt = b.expr_stmt(assign);
        // o.x
        let o_ref2 = b.name_expr("o");
        let read = b.member(o_ref2, "x");
        let read_stmt = b.expr_stmt(read);
        // delete o.x
        let o_ref3 = b.name_expr("o");
        let member = b.member(o_ref3, "x");
        let delete = b.expr(
            range(0, 0),
            Expression::Unary(UnaryExpression {
                operator: UnaryOperator::Delete,
                argument: Box::new(member),
            }),
        );
        let delete_stmt = b.expr_stmt(delete);
        let file = b.finish(
            ScriptKind::TypeScript,
            vec![decl, assign_stmt, read_stmt, delete_stmt],
        );
        let module = lower(&file, ts_options()).expect("member ops lower");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::SetProperty { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::GetProperty { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::DeleteProperty { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn function_call_uses_contiguous_argument_window() {
        let mut b = AstBuilder::new();
        let param_a = b.param("a");
        let param_bb = b.param("bb");
        let return_none = b.return_stmt(None);
        let f = b.function_stmt("f", vec![param_a, param_bb], vec![return_none]);
        let callee = b.name_expr("f");
        let one = b.number("1");
        let two = b.number("2");
        let call = b.call(callee, vec![one, two]);
        let call_stmt = b.expr_stmt(call);
        let file = b.finish(ScriptKind::JavaScript, vec![f, call_stmt]);
        let module = lower(&file, js_options()).expect("call lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        let call = entry
            .code()
            .iter()
            .find_map(|i| match i {
                Instruction::Call {
                    args_start,
                    arg_count,
                    ..
                } => Some((*args_start, *arg_count)),
                _ => None,
            })
            .expect("a Call is emitted");
        assert_eq!(call.1, 2);
        // f is a global function referenced from the entry (materialized).
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::DefineFunction { .. }))
        );
        assert!(module.functions().len() >= 2);
        assert_round_trips(&module);
    }

    #[test]
    fn method_call_passes_receiver_as_this() {
        let mut b = AstBuilder::new();
        let object = b.expr(
            range(0, 0),
            Expression::Object(ObjectLiteral {
                members: Vec::new(),
            }),
        );
        let decl = b.var_stmt(VariableKind::Const, "o", Some(object));
        let o_ref = b.name_expr("o");
        let callee = b.member(o_ref, "m");
        let call = b.call(callee, Vec::new());
        let call_stmt = b.expr_stmt(call);
        let file = b.finish(ScriptKind::TypeScript, vec![decl, call_stmt]);
        let module = lower(&file, ts_options()).expect("method call lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        // The receiver register (object) is both the GetProperty object and the
        // Call this_value.
        let get_object = entry.code().iter().find_map(|i| match i {
            Instruction::GetProperty { object, .. } => Some(*object),
            _ => None,
        });
        let this_value = entry.code().iter().find_map(|i| match i {
            Instruction::Call { this_value, .. } => Some(*this_value),
            _ => None,
        });
        assert_eq!(get_object, this_value);
        assert_round_trips(&module);
    }

    #[test]
    fn new_expression_lowers_to_construct() {
        let mut b = AstBuilder::new();
        let ctor_body = b.return_stmt(None);
        let ctor = b.function_stmt("C", Vec::new(), vec![ctor_body]);
        let callee = b.name_expr("C");
        let new = b.expr(
            range(0, 0),
            Expression::New(NewExpression {
                callee: Box::new(callee),
                type_arguments: None,
                arguments: Vec::new(),
            }),
        );
        let new_stmt = b.expr_stmt(new);
        let file = b.finish(ScriptKind::JavaScript, vec![ctor, new_stmt]);
        let module = lower(&file, js_options()).expect("new lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::Construct { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn function_with_parameters_binds_leading_registers() {
        let mut b = AstBuilder::new();
        let a_ref = b.name_expr("a");
        let bb_ref = b.name_expr("bb");
        let sum = b.add(a_ref, bb_ref);
        let ret = b.return_stmt(Some(sum));
        let param_a = b.param("a");
        let param_bb = b.param("bb");
        let function = b.function_stmt("add", vec![param_a, param_bb], vec![ret]);
        let file = b.finish(ScriptKind::TypeScript, vec![function]);
        let module = lower(&file, ts_options()).expect("parameters lower");
        let added = &module.functions()[1];
        assert_eq!(added.parameter_count(), 2);
        assert!(added.register_count() >= 2);
        assert_round_trips(&module);
    }

    #[test]
    fn recursive_function_resolves_its_own_name() {
        let mut b = AstBuilder::new();
        let n_ref = b.name_expr("recurse");
        let call = b.call(n_ref, Vec::new());
        let ret = b.return_stmt(Some(call));
        let function = b.function_stmt("recurse", Vec::new(), vec![ret]);
        let file = b.finish(ScriptKind::TypeScript, vec![function]);
        let module = lower(&file, ts_options()).expect("recursion resolves");
        assert_round_trips(&module);
    }

    #[test]
    fn arrow_expression_body_returns_its_value() {
        let mut b = AstBuilder::new();
        let a_ref = b.name_expr("a");
        let one = b.number("1");
        let body = b.add(a_ref, one);
        let arrow_param = b.param("a");
        let arrow = b.expr(
            range(0, 0),
            Expression::Arrow(ArrowFunction {
                is_async: false,
                type_parameters: None,
                parameters: vec![arrow_param],
                return_type: None,
                body: FunctionBody::Expression(Box::new(body)),
            }),
        );
        let statement = b.var_stmt(VariableKind::Const, "inc", Some(arrow));
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("arrow lowers");
        let arrow_fn = &module.functions()[1];
        assert_eq!(arrow_fn.parameter_count(), 1);
        assert!(
            arrow_fn
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::Return { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn await_lowers_to_suspend_with_fallthrough_resume() {
        let mut b = AstBuilder::new();
        let one = b.number("1");
        let range = one.range();
        let awaited = b.expr(
            range,
            Expression::Await(AwaitExpression {
                argument: Box::new(one),
            }),
        );
        let statement = b.expr_stmt(awaited);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("await lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        let suspend = entry.code().iter().enumerate().find_map(|(pc, i)| match i {
            Instruction::Suspend { resume, .. } => Some((pc as u32, *resume)),
            _ => None,
        });
        let (pc, resume) = suspend.expect("await emits Suspend");
        assert_eq!(resume.get(), pc + 1);
        assert_round_trips(&module);
    }

    #[test]
    fn decimal_bigint_literal_interns_a_bigint_constant() {
        let mut b = AstBuilder::new();
        let token = b.token(TokenKind::BigIntLiteral, "1_000n");
        let node = Node::new(
            b.id(),
            token.range(),
            crate::syntax::BigIntLiteral::new(token),
        );
        let literal = b.expr(token.range(), Expression::Literal(Literal::BigInt(node)));
        let statement = b.expr_stmt(literal);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("bigint lowers");
        assert!(
            module
                .constants()
                .iter()
                .any(|c| matches!(c, Constant::BigInt(_)))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn import_declaration_loads_module_and_named_bindings() {
        let mut b = AstBuilder::new();
        let source = b.string_node("mod");
        let default_ident = b.ident("def");
        let named_local = b.ident("named");
        let named_imported = b.ident("named");
        let specifier = Node::new(
            b.id(),
            range(0, 0),
            crate::syntax::ImportSpecifier {
                mode: crate::syntax::ImportSpecifierMode::Value,
                imported: crate::syntax::ModuleExportName::Identifier(named_imported),
                local: named_local,
            },
        );
        let clause = crate::syntax::ImportClause {
            default: Some(default_ident),
            binding: Some(crate::syntax::ImportBinding::Named(vec![specifier])),
        };
        let import = b.stmt(
            range(0, 0),
            Statement::Import(crate::syntax::ImportDeclaration {
                type_only: false,
                clause: Some(clause),
                source,
                attributes: None,
            }),
        );
        // Reference the imported bindings so they must resolve.
        let def_ref = b.name_expr("def");
        let named_ref = b.name_expr("named");
        let use_stmt = b.expr_stmt(def_ref);
        let use_stmt2 = b.expr_stmt(named_ref);
        let file = b.finish(ScriptKind::TypeScript, vec![import, use_stmt, use_stmt2]);
        let module = lower(&file, ts_options()).expect("import lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::Import { .. }))
        );
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::GetProperty { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn type_only_syntax_erases_structurally() {
        let mut b = AstBuilder::new();
        let interface_name = b.ident("Shape");
        let interface = b.stmt(
            interface_name.range(),
            Statement::Interface(crate::syntax::InterfaceDeclaration {
                name: interface_name,
                type_parameters: None,
                extends: Vec::new(),
                members: Vec::new(),
            }),
        );
        let one = b.number("1");
        let value = b.var_stmt(VariableKind::Const, "x", Some(one));
        let file = b.finish(ScriptKind::TypeScript, vec![interface, value]);
        let module = lower(&file, ts_options()).expect("type-only syntax erases");
        assert_eq!(module.constants(), &[Constant::Int32(1)]);
        assert_round_trips(&module);
    }

    #[test]
    fn indices_scale_beyond_127() {
        // 200 distinct string bindings force >127 constants and >127 registers,
        // proving the u32 index path and two-byte LEB wire encoding.
        let mut b = AstBuilder::new();
        let mut statements = Vec::new();
        for index in 0..200 {
            let value = b.string(&format!("s{index:03}"));
            statements.push(b.var_stmt(VariableKind::Const, &format!("v{index:03}"), Some(value)));
        }
        let file = b.finish(ScriptKind::TypeScript, statements);
        let module = lower(&file, ts_options()).expect("large module lowers");
        assert_eq!(module.constants().len(), 200);
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry.register_count() > 127,
            "registers exceed the old u8 ceiling"
        );
        // Round-trips through the wire, exercising multi-byte LEB indices.
        assert_round_trips(&module);
    }

    #[test]
    fn javascript_source_requires_compatibility_option() {
        let mut b = AstBuilder::new();
        let one = b.number("1");
        let stmt = b.var_stmt(VariableKind::Var, "x", Some(one));
        let file = b.finish(ScriptKind::JavaScript, vec![stmt]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::JavaScriptSourceNeedsCompatibility {
                script_kind: ScriptKind::JavaScript,
            }
        );
    }

    #[test]
    fn json_source_is_never_executable() {
        let b = AstBuilder::new();
        let file = b.finish(ScriptKind::Json, Vec::new());
        assert_eq!(
            error_kind(lower(&file, js_options())),
            LowerErrorKind::JsonSourceNotExecutable
        );
    }

    #[test]
    fn empty_module_verifies_with_a_halt() {
        let b = AstBuilder::new();
        let file = b.finish(ScriptKind::TypeScript, Vec::new());
        let module = lower(&file, ts_options()).expect("empty module lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert_eq!(entry.code(), &[Instruction::Halt]);
        assert_eq!(entry.register_count(), 0);
        assert!(module.constants().is_empty());
    }

    #[test]
    fn computed_member_access_is_unexpressible() {
        let mut b = AstBuilder::new();
        let object = b.name_expr("undefined");
        let index = b.name_expr("undefined");
        let member = b.expr(
            range(0, 0),
            Expression::Member(MemberExpression {
                object: Box::new(object),
                property: MemberProperty::Computed(Box::new(index)),
                optional: false,
            }),
        );
        let statement = b.expr_stmt(member);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::ComputedMemberAccess)
        );
    }

    #[test]
    fn array_literal_with_elements_is_unexpressible() {
        let mut b = AstBuilder::new();
        let one = b.number("1");
        let array = b.expr(
            range(0, 0),
            Expression::Array(ArrayLiteral {
                elements: vec![crate::syntax::ArrayElement::Expression(Box::new(one))],
            }),
        );
        let statement = b.expr_stmt(array);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::ArrayElements)
        );
    }

    #[test]
    fn empty_array_literal_is_expressible() {
        let mut b = AstBuilder::new();
        let array = b.expr(
            range(0, 0),
            Expression::Array(ArrayLiteral {
                elements: Vec::new(),
            }),
        );
        let statement = b.var_stmt(VariableKind::Const, "a", Some(array));
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        let module = lower(&file, ts_options()).expect("empty array lowers");
        let entry = &module.functions()[module.entry().get() as usize];
        assert!(
            entry
                .code()
                .iter()
                .any(|i| matches!(i, Instruction::CreateArray { .. }))
        );
        assert_round_trips(&module);
    }

    #[test]
    fn capturing_closure_is_a_typed_error() {
        let mut b = AstBuilder::new();
        let one = b.number("1");
        let outer = b.var_stmt(VariableKind::Let, "captured", Some(one));
        let captured_ref = b.name_expr("captured");
        let use_stmt = b.expr_stmt(captured_ref);
        let function = b.function_stmt("f", Vec::new(), vec![use_stmt]);
        let file = b.finish(ScriptKind::TypeScript, vec![outer, function]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::ClosureCapture {
                name: "captured".to_owned(),
            }
        );
    }

    #[test]
    fn unresolved_identifier_is_distinct_from_capture() {
        let mut b = AstBuilder::new();
        let ghost = b.name_expr("ghost");
        let statement = b.expr_stmt(ghost);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::UnresolvedIdentifier {
                name: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn top_level_return_is_a_typed_error() {
        let mut b = AstBuilder::new();
        let return_stmt = b.return_stmt(None);
        let file = b.finish(ScriptKind::TypeScript, vec![return_stmt]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::ReturnOutsideFunction)
        );
    }

    #[test]
    fn class_declaration_is_unexpressible() {
        let mut b = AstBuilder::new();
        let class_name = b.ident("C");
        let class = b.stmt(
            range(0, 0),
            Statement::Class(crate::syntax::ClassDeclaration {
                decorators: Vec::new(),
                modifiers: crate::syntax::DeclarationModifiers::default(),
                name: Some(class_name),
                type_parameters: None,
                extends: None,
                implements: Vec::new(),
                members: Vec::new(),
            }),
        );
        let file = b.finish(ScriptKind::TypeScript, vec![class]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::ClassDeclaration)
        );
    }

    #[test]
    fn escaped_string_literal_is_rejected_not_miscooked() {
        let mut b = AstBuilder::new();
        let token = b.token(TokenKind::StringLiteral, "\"a\\nb\"");
        let node = Node::new(b.id(), token.range(), StringLiteral::new(token));
        let literal = b.expr(token.range(), Expression::Literal(Literal::String(node)));
        let statement = b.expr_stmt(literal);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Unsupported(UnsupportedConstruct::EscapedStringLiteral)
        );
    }

    #[test]
    fn string_constants_respect_the_pool_byte_ceiling() {
        let mut b = AstBuilder::new();
        let oversized = "x".repeat(super::MAX_STRING_BYTES + 1);
        let literal = b.string(&oversized);
        let statement = b.expr_stmt(literal);
        let file = b.finish(ScriptKind::TypeScript, vec![statement]);
        assert_eq!(
            error_kind(lower(&file, ts_options())),
            LowerErrorKind::Capacity(CapacityLimit::StringBytes)
        );
    }
}
