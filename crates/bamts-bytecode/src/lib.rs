//! Production BamTS bytecode: a verified instruction set, strict codec, and
//! definite-initialization verifier with an unforgeable `Verified` typestate.
//!
//! # Relation to the formal five-op core
//!
//! The proven structural core in `formal/lean/Bamti/Bytecode/Model.lean`
//! (`Load`, `Add`, `Jump`, `Suspend`, `Halt`) is preserved exactly as a
//! *subset* of this ISA:
//!
//! * `Load`    -> [`Instruction::LoadConst`] (now names the loaded constant).
//! * `Add`     -> [`Instruction::Binary`] with [`BinaryOp::Add`] (generalized
//!   to the full closed operator algebra).
//! * `Jump`    -> [`Instruction::Jump`] (identical control transfer).
//! * `Suspend` -> [`Instruction::Suspend`] (refined with an out register for
//!   the resumed value and an in register for the yielded value).
//! * `Halt`    -> [`Instruction::Halt`] (identical terminator).
//!
//! Every additional opcode *extends* that core. The verifier here mirrors the
//! *structure* proven in `Bytecode/Verify.lean` -- a real entry, valid CFG
//! targets, nested (non-partially-overlapping) handlers, and a syntactic
//! definite-initialization witness across CFG joins -- before the `Verified`
//! typestate can be constructed. Number constants use `Bamti.canonical_nan`,
//! and persisted constants carry no heap or runtime identity, matching
//! `no_serialized_runtime_identity`.
//!
//! The wire format is a deliberate superset departure from the formal single
//! seven-bit-group encoding: integer fields are canonical unsigned LEB128 `u32`
//! (functions and modules may exceed 127 instructions, constants, and
//! registers), bounded by explicit decode and structural resource limits. Its
//! round-trip guarantees -- totality over hostile bytes, canonical re-encoding,
//! and decode/encode identity -- are fresh properties of this codec, proven by
//! the tests in this module, not the Lean single-byte theorems (`decode_total`,
//! `decode_encode_canonical`, `encode_decode_identity`), which remain scoped to
//! the formal five-op wire.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

/// `BMTBC\0\0\1`, matching `Bamti.Bytecode.magicBytes`.
pub const MAGIC: [u8; 8] = [66, 77, 84, 66, 67, 0, 0, 1];
/// The sole supported wire version.
pub const FORMAT_VERSION: u8 = 1;

/// Structural verify-time ceiling on a function's register count. Generous
/// enough for real code yet bounds definite-initialization bitset allocation.
pub const MAX_REGISTERS: u32 = 1 << 16;
/// Structural verify-time ceiling on a function's instruction count.
pub const MAX_INSTRUCTIONS: u32 = 1 << 20;
/// Structural verify-time ceiling on a function's handler count.
pub const MAX_HANDLERS: u32 = 1 << 16;
/// Structural verify-time ceiling on a module's constant count.
pub const MAX_CONSTANTS: u32 = 1 << 20;
/// Structural verify-time ceiling on a module's function count.
pub const MAX_FUNCTIONS: u32 = 1 << 20;

/// Combined verify-time ceiling on the total definite-initialization fact
/// storage a module may force the verifier to allocate, in 64-bit words. Each
/// function needs `instructions * ceil(registers / 64)` words, so independent
/// per-function maxima (`MAX_INSTRUCTIONS * (MAX_REGISTERS / 64)` alone is
/// 2^30 words = 8 GiB, and modules hold many functions) would permit multi-GiB
/// allocations from untrusted input. This bound caps that transient storage at
/// `MAX_VERIFIER_FACTS_WORDS * 8` bytes (64 MiB) while remaining generous
/// enough for large real modules with far more than 127 functions.
pub const MAX_VERIFIER_FACTS_WORDS: u64 = 1 << 23;

const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;
const EXPONENT_MASK: u64 = 0x7ff0_0000_0000_0000;
const FRACTION_MASK: u64 = 0x000f_ffff_ffff_ffff;

macro_rules! index_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u32);

        impl $name {
            #[must_use]
            pub const fn new(raw: u32) -> Self {
                Self(raw)
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }
    };
}

index_type!(
    /// Index of a virtual register within a function's register file.
    Register
);
index_type!(
    /// Index into a module's constant pool.
    ConstantId
);
index_type!(
    /// Index into a module's function table.
    FunctionId
);
index_type!(
    /// A program counter: an instruction index within a function's code.
    Pc
);

/// Canonical IEEE-754 bits. Every positive or negative NaN payload collapses
/// to the unique arithmetic NaN from `Bamti.canonical_nan`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct NumberBits(u64);

impl NumberBits {
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        if is_nan(bits) {
            Self(CANONICAL_NAN_BITS)
        } else {
            Self(bits)
        }
    }

    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self::from_bits(value.to_bits())
    }

    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn to_f64(self) -> f64 {
        f64::from_bits(self.0)
    }

    const fn from_wire(bits: u64) -> Option<Self> {
        if is_nan(bits) && bits != CANONICAL_NAN_BITS {
            None
        } else {
            Some(Self(bits))
        }
    }
}

const fn is_nan(bits: u64) -> bool {
    bits & EXPONENT_MASK == EXPONENT_MASK && bits & FRACTION_MASK != 0
}

/// A canonical BigInt literal in decimal text form. Constructed only through
/// [`BigIntLiteral::new`], so a `BigIntLiteral` value is *always* a canonical
/// decimal integer: optional leading `-`, no redundant leading zeros, no `-0`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BigIntLiteral(String);

impl BigIntLiteral {
    /// Parses canonical decimal text, rejecting empty text, non-digits,
    /// redundant leading zeros, a bare sign, and negative zero.
    #[must_use]
    pub fn new(text: String) -> Option<Self> {
        if is_canonical_bigint(&text) {
            Some(Self(text))
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_canonical_bigint(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let negative = bytes[0] == b'-';
    let digits = if negative { &bytes[1..] } else { bytes };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    // No redundant leading zero (e.g. "007", "00").
    if digits.len() > 1 && digits[0] == b'0' {
        return false;
    }
    // No "-0".
    !(negative && digits == b"0")
}

/// Persistable values. Heap references, holes, and uninitialized sentinels are
/// intentionally absent because they are runtime identities/states. Property
/// keys are not a distinct variant: instructions reference an existing
/// [`Constant::String`] by [`ConstantId`], reusing the string pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constant {
    Number(NumberBits),
    Int32(i32),
    String(String),
    Boolean(bool),
    Null,
    Undefined,
    BigInt(BigIntLiteral),
}

/// Closed set of unary operators.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnaryOp {
    Void,
    TypeOf,
    Plus,
    Negate,
    BitwiseNot,
    LogicalNot,
}

impl UnaryOp {
    const fn to_u8(self) -> u8 {
        match self {
            Self::Void => 0,
            Self::TypeOf => 1,
            Self::Plus => 2,
            Self::Negate => 3,
            Self::BitwiseNot => 4,
            Self::LogicalNot => 5,
        }
    }

    const fn from_u8(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Void),
            1 => Some(Self::TypeOf),
            2 => Some(Self::Plus),
            3 => Some(Self::Negate),
            4 => Some(Self::BitwiseNot),
            5 => Some(Self::LogicalNot),
            _ => None,
        }
    }
}

/// Closed set of binary operators. [`BinaryOp::Add`] is the formal core's `Add`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Exponent,
    BitAnd,
    BitOr,
    BitXor,
    ShiftLeft,
    ShiftRight,
    UnsignedShiftRight,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    InstanceOf,
    In,
}

impl BinaryOp {
    const fn to_u8(self) -> u8 {
        match self {
            Self::Add => 0,
            Self::Subtract => 1,
            Self::Multiply => 2,
            Self::Divide => 3,
            Self::Remainder => 4,
            Self::Exponent => 5,
            Self::BitAnd => 6,
            Self::BitOr => 7,
            Self::BitXor => 8,
            Self::ShiftLeft => 9,
            Self::ShiftRight => 10,
            Self::UnsignedShiftRight => 11,
            Self::Equal => 12,
            Self::NotEqual => 13,
            Self::StrictEqual => 14,
            Self::StrictNotEqual => 15,
            Self::LessThan => 16,
            Self::LessThanOrEqual => 17,
            Self::GreaterThan => 18,
            Self::GreaterThanOrEqual => 19,
            Self::InstanceOf => 20,
            Self::In => 21,
        }
    }

    const fn from_u8(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Add),
            1 => Some(Self::Subtract),
            2 => Some(Self::Multiply),
            3 => Some(Self::Divide),
            4 => Some(Self::Remainder),
            5 => Some(Self::Exponent),
            6 => Some(Self::BitAnd),
            7 => Some(Self::BitOr),
            8 => Some(Self::BitXor),
            9 => Some(Self::ShiftLeft),
            10 => Some(Self::ShiftRight),
            11 => Some(Self::UnsignedShiftRight),
            12 => Some(Self::Equal),
            13 => Some(Self::NotEqual),
            14 => Some(Self::StrictEqual),
            15 => Some(Self::StrictNotEqual),
            16 => Some(Self::LessThan),
            17 => Some(Self::LessThanOrEqual),
            18 => Some(Self::GreaterThan),
            19 => Some(Self::GreaterThanOrEqual),
            20 => Some(Self::InstanceOf),
            21 => Some(Self::In),
            _ => None,
        }
    }
}

/// The production instruction algebra. Opcodes 0..=19 are stable wire tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Instruction {
    /// Load a constant into `dst` (refines the formal `Load`).
    LoadConst { dst: Register, constant: ConstantId },
    /// Copy `src` into `dst`.
    Move { dst: Register, src: Register },
    /// Apply a unary operator to `operand`, writing `dst`.
    Unary {
        dst: Register,
        op: UnaryOp,
        operand: Register,
    },
    /// Apply a binary operator (generalizes the formal `Add`), writing `dst`.
    Binary {
        dst: Register,
        op: BinaryOp,
        left: Register,
        right: Register,
    },
    /// Create a fresh empty object in `dst`.
    CreateObject { dst: Register },
    /// Create a fresh empty array in `dst`.
    CreateArray { dst: Register },
    /// Materialize a closure over the named function into `dst`.
    DefineFunction { dst: Register, function: FunctionId },
    /// `dst = object[key]`, where `key` names a string constant.
    GetProperty {
        dst: Register,
        object: Register,
        key: ConstantId,
    },
    /// `object[key] = value`, where `key` names a string constant.
    SetProperty {
        object: Register,
        key: ConstantId,
        value: Register,
    },
    /// `dst = delete object[key]`, where `key` names a string constant.
    DeleteProperty {
        dst: Register,
        object: Register,
        key: ConstantId,
    },
    /// Call `callee` with `this_value` and the argument window
    /// `[args_start, args_start + arg_count)`, writing the result to `dst`.
    Call {
        dst: Register,
        callee: Register,
        this_value: Register,
        args_start: Register,
        arg_count: u32,
    },
    /// Construct with `callee` and the argument window
    /// `[args_start, args_start + arg_count)`, writing the instance to `dst`.
    Construct {
        dst: Register,
        callee: Register,
        args_start: Register,
        arg_count: u32,
    },
    /// Unconditional control transfer (identical to the formal `Jump`).
    Jump { target: Pc },
    /// Branch to `target` when `condition` is truthy, else fall through.
    JumpIfTrue { condition: Register, target: Pc },
    /// Branch to `target` when `condition` is falsy, else fall through.
    JumpIfFalse { condition: Register, target: Pc },
    /// Return `value` to the caller (terminator).
    Return { value: Register },
    /// Throw `value` (terminator; caught by an enclosing handler if any).
    Throw { value: Register },
    /// Yield `src` and resume at `resume`, receiving the resumed value in `dst`
    /// (refines the formal `Suspend`).
    Suspend {
        dst: Register,
        src: Register,
        resume: Pc,
    },
    /// Import the module named by the string constant `specifier` into `dst`.
    Import {
        dst: Register,
        specifier: ConstantId,
    },
    /// Terminate the current activation (identical to the formal `Halt`).
    Halt,
}

impl Instruction {
    /// Visits every register this instruction reads before executing.
    fn visit_reads(self, mut visit: impl FnMut(Register)) {
        match self {
            Self::Move { src, .. } => visit(src),
            Self::Unary { operand, .. } => visit(operand),
            Self::Binary { left, right, .. } => {
                visit(left);
                visit(right);
            }
            Self::GetProperty { object, .. } | Self::DeleteProperty { object, .. } => visit(object),
            Self::SetProperty { object, value, .. } => {
                visit(object);
                visit(value);
            }
            Self::Call {
                callee,
                this_value,
                args_start,
                arg_count,
                ..
            } => {
                visit(callee);
                visit(this_value);
                for offset in 0..arg_count {
                    visit(Register::new(args_start.get() + offset));
                }
            }
            Self::Construct {
                callee,
                args_start,
                arg_count,
                ..
            } => {
                visit(callee);
                for offset in 0..arg_count {
                    visit(Register::new(args_start.get() + offset));
                }
            }
            Self::JumpIfTrue { condition, .. } | Self::JumpIfFalse { condition, .. } => {
                visit(condition);
            }
            Self::Return { value } | Self::Throw { value } => visit(value),
            Self::Suspend { src, .. } => visit(src),
            Self::LoadConst { .. }
            | Self::CreateObject { .. }
            | Self::CreateArray { .. }
            | Self::DefineFunction { .. }
            | Self::Jump { .. }
            | Self::Import { .. }
            | Self::Halt => {}
        }
    }

    /// The single register this instruction defines, if any.
    const fn write(self) -> Option<Register> {
        match self {
            Self::LoadConst { dst, .. }
            | Self::Move { dst, .. }
            | Self::Unary { dst, .. }
            | Self::Binary { dst, .. }
            | Self::CreateObject { dst }
            | Self::CreateArray { dst }
            | Self::DefineFunction { dst, .. }
            | Self::GetProperty { dst, .. }
            | Self::DeleteProperty { dst, .. }
            | Self::Call { dst, .. }
            | Self::Construct { dst, .. }
            | Self::Suspend { dst, .. }
            | Self::Import { dst, .. } => Some(dst),
            Self::SetProperty { .. }
            | Self::Jump { .. }
            | Self::JumpIfTrue { .. }
            | Self::JumpIfFalse { .. }
            | Self::Return { .. }
            | Self::Throw { .. }
            | Self::Halt => None,
        }
    }

    /// Visits each normal-control successor. Terminators visit nothing, which
    /// is exactly how reachable fall-off is forbidden: any non-terminator whose
    /// fall-through `pc + 1` equals the code length fails target verification.
    fn visit_successors(self, pc: u32, mut visit: impl FnMut(Pc)) {
        match self {
            Self::Jump { target } => visit(target),
            Self::JumpIfTrue { target, .. } | Self::JumpIfFalse { target, .. } => {
                visit(target);
                visit(Pc::new(pc + 1));
            }
            Self::Suspend { resume, .. } => visit(resume),
            Self::Return { .. } | Self::Throw { .. } | Self::Halt => {}
            Self::LoadConst { .. }
            | Self::Move { .. }
            | Self::Unary { .. }
            | Self::Binary { .. }
            | Self::CreateObject { .. }
            | Self::CreateArray { .. }
            | Self::DefineFunction { .. }
            | Self::GetProperty { .. }
            | Self::SetProperty { .. }
            | Self::DeleteProperty { .. }
            | Self::Call { .. }
            | Self::Construct { .. }
            | Self::Import { .. } => visit(Pc::new(pc + 1)),
        }
    }
}

/// A half-open protected range `[start, end)`, its handler entry PC, and the
/// register that receives the thrown value on dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExceptionHandler {
    pub start: Pc,
    pub end: Pc,
    pub handler: Pc,
    pub catch_register: Register,
}

/// Compact function flags record.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct FunctionFlags {
    pub is_async: bool,
    pub is_generator: bool,
}

impl FunctionFlags {
    const ASYNC: u8 = 0b01;
    const GENERATOR: u8 = 0b10;
    const KNOWN: u8 = Self::ASYNC | Self::GENERATOR;

    const fn to_bits(self) -> u8 {
        let mut bits = 0;
        if self.is_async {
            bits |= Self::ASYNC;
        }
        if self.is_generator {
            bits |= Self::GENERATOR;
        }
        bits
    }

    const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN != 0 {
            return None;
        }
        Some(Self {
            is_async: bits & Self::ASYNC != 0,
            is_generator: bits & Self::GENERATOR != 0,
        })
    }
}

/// An explicit function record: metadata, code, and handlers. The first
/// `parameter_count` registers are the parameters and are initialized on entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    name: Option<ConstantId>,
    parameter_count: u32,
    register_count: u32,
    flags: FunctionFlags,
    code: Vec<Instruction>,
    handlers: Vec<ExceptionHandler>,
}

impl Function {
    #[must_use]
    pub fn new(
        name: Option<ConstantId>,
        parameter_count: u32,
        register_count: u32,
        flags: FunctionFlags,
        code: Vec<Instruction>,
        handlers: Vec<ExceptionHandler>,
    ) -> Self {
        Self {
            name,
            parameter_count,
            register_count,
            flags,
            code,
            handlers,
        }
    }

    #[must_use]
    pub const fn name(&self) -> Option<ConstantId> {
        self.name
    }

    #[must_use]
    pub const fn parameter_count(&self) -> u32 {
        self.parameter_count
    }

    #[must_use]
    pub const fn register_count(&self) -> u32 {
        self.register_count
    }

    #[must_use]
    pub const fn flags(&self) -> FunctionFlags {
        self.flags
    }

    #[must_use]
    pub fn code(&self) -> &[Instruction] {
        &self.code
    }

    #[must_use]
    pub fn handlers(&self) -> &[ExceptionHandler] {
        &self.handlers
    }
}

/// Marker for decoded or newly assembled, untrusted bytecode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unverified {
    _private: (),
}

/// Unforgeable marker proving verification completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verified {
    _private: (),
}

/// Explicit constant pool, function table, and entry function, with typestate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Module<State = Unverified> {
    constants: Vec<Constant>,
    functions: Vec<Function>,
    entry: FunctionId,
    certificates: Vec<Certificate>,
    state: PhantomData<State>,
}

impl<State> Module<State> {
    #[must_use]
    pub fn constants(&self) -> &[Constant] {
        &self.constants
    }

    #[must_use]
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    #[must_use]
    pub const fn entry(&self) -> FunctionId {
        self.entry
    }
}

impl Module<Unverified> {
    #[must_use]
    pub fn new(constants: Vec<Constant>, functions: Vec<Function>, entry: FunctionId) -> Self {
        Self {
            constants,
            functions,
            entry,
            certificates: Vec::new(),
            state: PhantomData,
        }
    }

    /// Consumes untrusted structure and is the only route to `Module<Verified>`.
    ///
    /// # Errors
    /// Returns the first structural violation found (bounds, references, CFG,
    /// handlers, or definite initialization).
    pub fn verify(self) -> Result<Module<Verified>, VerifyError> {
        verify_module(self)
    }
}

impl Module<Verified> {
    #[must_use]
    pub fn certificate(&self, function: FunctionId) -> Option<&Certificate> {
        self.certificates.get(function.get() as usize)
    }

    /// Emits one deterministic canonical representation.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&MAGIC);
        output.push(FORMAT_VERSION);
        write_u32(self.constants.len() as u32, &mut output);
        for constant in &self.constants {
            encode_constant(constant, &mut output);
        }
        write_u32(self.functions.len() as u32, &mut output);
        write_u32(self.entry.get(), &mut output);
        for function in &self.functions {
            encode_function(function, &mut output);
        }
        output
    }
}

/// Forward-dataflow definite-initialization facts, mirroring Lean's
/// `Certificate.facts`. Construction is private, so certificates are unforgeable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Certificate {
    register_count: u32,
    facts: Vec<RegisterSet>,
}

impl Certificate {
    #[must_use]
    pub fn instruction_count(&self) -> usize {
        self.facts.len()
    }

    /// Whether `register` is definitely initialized before executing `pc`.
    /// Total for every wrapper value: out-of-range registers or PCs yield
    /// `None` rather than panicking.
    #[must_use]
    pub fn initialized_before(&self, pc: Pc, register: Register) -> Option<bool> {
        if register.get() >= self.register_count {
            return None;
        }
        self.facts
            .get(pc.get() as usize)
            .map(|facts| facts.contains(register))
    }
}

/// A dynamically sized register bitset covering a function's register file.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisterSet {
    words: Box<[u64]>,
}

impl RegisterSet {
    fn words_for(register_count: u32) -> usize {
        (register_count as usize).div_ceil(64)
    }

    fn empty(register_count: u32) -> Self {
        Self {
            words: vec![0; Self::words_for(register_count)].into_boxed_slice(),
        }
    }

    fn full(register_count: u32) -> Self {
        let words_len = Self::words_for(register_count);
        let mut words = vec![u64::MAX; words_len];
        let remainder = register_count % 64;
        if remainder != 0 && words_len != 0 {
            words[words_len - 1] = (1_u64 << remainder) - 1;
        }
        Self {
            words: words.into_boxed_slice(),
        }
    }

    fn contains(&self, register: Register) -> bool {
        let index = register.get() as usize;
        self.words
            .get(index / 64)
            .is_some_and(|word| word & (1_u64 << (index % 64)) != 0)
    }

    fn insert(&mut self, register: Register) {
        let index = register.get() as usize;
        if let Some(word) = self.words.get_mut(index / 64) {
            *word |= 1_u64 << (index % 64);
        }
    }

    fn insert_prefix(&mut self, count: u32) {
        for register in 0..count {
            self.insert(Register::new(register));
        }
    }

    fn intersect(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (slot, mask) in self.words.iter_mut().zip(other.words.iter()) {
            let next = *slot & *mask;
            if next != *slot {
                changed = true;
                *slot = next;
            }
        }
        changed
    }
}

/// A structural verification failure, located at a function and/or instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifyError {
    pub function: Option<FunctionId>,
    pub instruction: Option<Pc>,
    pub kind: VerifyErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyErrorKind {
    EmptyModule,
    VerifierWorkLimitExceeded {
        work: u64,
        limit: u64,
    },
    TooManyConstants {
        count: usize,
    },
    TooManyFunctions {
        count: usize,
    },
    EntryFunctionOutOfBounds {
        entry: u32,
        function_count: usize,
    },
    EmptyFunction,
    TooManyInstructions {
        count: usize,
    },
    TooManyHandlers {
        count: usize,
    },
    RegisterCountOutOfBounds {
        count: u32,
    },
    ParameterCountExceedsRegisters {
        parameter_count: u32,
        register_count: u32,
    },
    FunctionNameOutOfBounds {
        constant: u32,
    },
    FunctionNameNotString {
        constant: ConstantId,
    },
    RegisterOutOfBounds {
        register: Register,
        register_count: u32,
    },
    ConstantOutOfBounds {
        constant: ConstantId,
        constant_count: usize,
    },
    PropertyKeyNotString {
        constant: ConstantId,
    },
    FunctionReferenceOutOfBounds {
        function: FunctionId,
        function_count: usize,
    },
    ArgumentWindowOutOfBounds {
        args_start: Register,
        arg_count: u32,
        register_count: u32,
    },
    JumpOutOfBounds {
        target: u32,
        instruction_count: usize,
    },
    InvalidHandlerBounds {
        handler: usize,
        start: u32,
        end: u32,
        target: u32,
    },
    HandlerCatchRegisterOutOfBounds {
        handler: usize,
        register: Register,
        register_count: u32,
    },
    HandlersPartiallyOverlap {
        left: usize,
        right: usize,
    },
    ReadBeforeWrite {
        register: Register,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(function) = self.function {
            write!(formatter, "function {}", function.get())?;
            if let Some(instruction) = self.instruction {
                write!(formatter, " instruction {}", instruction.get())?;
            }
            formatter.write_str(": ")?;
        }
        match &self.kind {
            VerifyErrorKind::EmptyModule => formatter.write_str("module has no functions"),
            VerifyErrorKind::VerifierWorkLimitExceeded { work, limit } => write!(
                formatter,
                "verifier fact allocation {work} words exceeds the {limit}-word limit"
            ),
            VerifyErrorKind::TooManyConstants { count } => {
                write!(formatter, "{count} constants exceed the structural limit")
            }
            VerifyErrorKind::TooManyFunctions { count } => {
                write!(formatter, "{count} functions exceed the structural limit")
            }
            VerifyErrorKind::EntryFunctionOutOfBounds {
                entry,
                function_count,
            } => write!(
                formatter,
                "entry function {entry} is outside {function_count} functions"
            ),
            VerifyErrorKind::EmptyFunction => {
                formatter.write_str("function has no entry instruction")
            }
            VerifyErrorKind::TooManyInstructions { count } => {
                write!(
                    formatter,
                    "{count} instructions exceed the structural limit"
                )
            }
            VerifyErrorKind::TooManyHandlers { count } => {
                write!(formatter, "{count} handlers exceed the structural limit")
            }
            VerifyErrorKind::RegisterCountOutOfBounds { count } => {
                write!(
                    formatter,
                    "register count {count} exceeds the structural limit"
                )
            }
            VerifyErrorKind::ParameterCountExceedsRegisters {
                parameter_count,
                register_count,
            } => write!(
                formatter,
                "parameter count {parameter_count} exceeds register count {register_count}"
            ),
            VerifyErrorKind::FunctionNameOutOfBounds { constant } => {
                write!(
                    formatter,
                    "function name constant {constant} is out of bounds"
                )
            }
            VerifyErrorKind::FunctionNameNotString { constant } => write!(
                formatter,
                "function name constant {} is not a string",
                constant.get()
            ),
            VerifyErrorKind::RegisterOutOfBounds {
                register,
                register_count,
            } => write!(
                formatter,
                "register {} is outside register count {register_count}",
                register.get()
            ),
            VerifyErrorKind::ConstantOutOfBounds {
                constant,
                constant_count,
            } => write!(
                formatter,
                "constant {} is outside {constant_count} constants",
                constant.get()
            ),
            VerifyErrorKind::PropertyKeyNotString { constant } => write!(
                formatter,
                "property key constant {} is not a string",
                constant.get()
            ),
            VerifyErrorKind::FunctionReferenceOutOfBounds {
                function,
                function_count,
            } => write!(
                formatter,
                "function reference {} is outside {function_count} functions",
                function.get()
            ),
            VerifyErrorKind::ArgumentWindowOutOfBounds {
                args_start,
                arg_count,
                register_count,
            } => write!(
                formatter,
                "argument window [{}, {} + {arg_count}) escapes register count {register_count}",
                args_start.get(),
                args_start.get()
            ),
            VerifyErrorKind::JumpOutOfBounds {
                target,
                instruction_count,
            } => write!(
                formatter,
                "target {target} is not one of {instruction_count} instruction boundaries"
            ),
            VerifyErrorKind::InvalidHandlerBounds {
                handler,
                start,
                end,
                target,
            } => write!(
                formatter,
                "handler {handler} has invalid range {start}..{end} or target {target}"
            ),
            VerifyErrorKind::HandlerCatchRegisterOutOfBounds {
                handler,
                register,
                register_count,
            } => write!(
                formatter,
                "handler {handler} catch register {} is outside register count {register_count}",
                register.get()
            ),
            VerifyErrorKind::HandlersPartiallyOverlap { left, right } => write!(
                formatter,
                "handlers {left} and {right} partially overlap instead of nesting"
            ),
            VerifyErrorKind::ReadBeforeWrite { register } => write!(
                formatter,
                "register {} may be read before initialization",
                register.get()
            ),
        }
    }
}

impl Error for VerifyError {}

fn module_error(kind: VerifyErrorKind) -> VerifyError {
    VerifyError {
        function: None,
        instruction: None,
        kind,
    }
}

fn function_error(function: usize, kind: VerifyErrorKind) -> VerifyError {
    VerifyError {
        function: Some(FunctionId::new(function as u32)),
        instruction: None,
        kind,
    }
}

fn instruction_error(function: usize, pc: usize, kind: VerifyErrorKind) -> VerifyError {
    VerifyError {
        function: Some(FunctionId::new(function as u32)),
        instruction: Some(Pc::new(pc as u32)),
        kind,
    }
}

fn verify_module(module: Module<Unverified>) -> Result<Module<Verified>, VerifyError> {
    if module.functions.is_empty() {
        return Err(module_error(VerifyErrorKind::EmptyModule));
    }
    if module.constants.len() as u64 > u64::from(MAX_CONSTANTS) {
        return Err(module_error(VerifyErrorKind::TooManyConstants {
            count: module.constants.len(),
        }));
    }
    if module.functions.len() as u64 > u64::from(MAX_FUNCTIONS) {
        return Err(module_error(VerifyErrorKind::TooManyFunctions {
            count: module.functions.len(),
        }));
    }
    if module.entry.get() as usize >= module.functions.len() {
        return Err(module_error(VerifyErrorKind::EntryFunctionOutOfBounds {
            entry: module.entry.get(),
            function_count: module.functions.len(),
        }));
    }

    // Combined, overflow-safe cap on the definite-initialization fact storage
    // this module can force the verifier to allocate, checked before any
    // per-function allocation. Saturating arithmetic can only tighten the
    // bound, so it never masks an over-limit module. The comparison is
    // strict `>`: facts-work equal to the cap is the largest permitted
    // allocation. Acceptance at exactly the cap is intentionally untested
    // because it forces the full 64 MiB `facts` allocation; the boundary is
    // pinned from the rejection side only (see `verifier_rejects_facts_work_
    // above_cap`).
    let mut total_facts_words: u64 = 0;
    for function in &module.functions {
        let words = RegisterSet::words_for(function.register_count) as u64;
        let function_words = (function.code.len() as u64).saturating_mul(words);
        total_facts_words = total_facts_words.saturating_add(function_words);
        if total_facts_words > MAX_VERIFIER_FACTS_WORDS {
            return Err(module_error(VerifyErrorKind::VerifierWorkLimitExceeded {
                work: total_facts_words,
                limit: MAX_VERIFIER_FACTS_WORDS,
            }));
        }
    }

    let mut certificates = Vec::with_capacity(module.functions.len());
    for (index, function) in module.functions.iter().enumerate() {
        certificates.push(verify_function(&module, index, function)?);
    }

    Ok(Module {
        constants: module.constants,
        functions: module.functions,
        entry: module.entry,
        certificates,
        state: PhantomData,
    })
}

fn verify_function(
    module: &Module<Unverified>,
    function_index: usize,
    function: &Function,
) -> Result<Certificate, VerifyError> {
    if function.code.is_empty() {
        return Err(function_error(
            function_index,
            VerifyErrorKind::EmptyFunction,
        ));
    }
    if function.code.len() as u64 > u64::from(MAX_INSTRUCTIONS) {
        return Err(function_error(
            function_index,
            VerifyErrorKind::TooManyInstructions {
                count: function.code.len(),
            },
        ));
    }
    if function.handlers.len() as u64 > u64::from(MAX_HANDLERS) {
        return Err(function_error(
            function_index,
            VerifyErrorKind::TooManyHandlers {
                count: function.handlers.len(),
            },
        ));
    }
    if function.register_count > MAX_REGISTERS {
        return Err(function_error(
            function_index,
            VerifyErrorKind::RegisterCountOutOfBounds {
                count: function.register_count,
            },
        ));
    }
    if function.parameter_count > function.register_count {
        return Err(function_error(
            function_index,
            VerifyErrorKind::ParameterCountExceedsRegisters {
                parameter_count: function.parameter_count,
                register_count: function.register_count,
            },
        ));
    }
    verify_function_name(module, function_index, function)?;
    verify_handlers(function_index, function)?;
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        verify_instruction(module, function_index, function, pc, instruction)?;
    }
    definite_initialization(function_index, function)
}

fn verify_function_name(
    module: &Module<Unverified>,
    function_index: usize,
    function: &Function,
) -> Result<(), VerifyError> {
    let Some(name) = function.name else {
        return Ok(());
    };
    let Some(constant) = module.constants.get(name.get() as usize) else {
        return Err(function_error(
            function_index,
            VerifyErrorKind::FunctionNameOutOfBounds {
                constant: name.get(),
            },
        ));
    };
    if !matches!(constant, Constant::String(_)) {
        return Err(function_error(
            function_index,
            VerifyErrorKind::FunctionNameNotString { constant: name },
        ));
    }
    Ok(())
}

fn verify_handlers(function_index: usize, function: &Function) -> Result<(), VerifyError> {
    let code_len = function.code.len();
    for (index, handler) in function.handlers.iter().copied().enumerate() {
        if handler.start.get() >= handler.end.get()
            || handler.end.get() as usize > code_len
            || handler.handler.get() as usize >= code_len
        {
            return Err(function_error(
                function_index,
                VerifyErrorKind::InvalidHandlerBounds {
                    handler: index,
                    start: handler.start.get(),
                    end: handler.end.get(),
                    target: handler.handler.get(),
                },
            ));
        }
        if handler.catch_register.get() >= function.register_count {
            return Err(function_error(
                function_index,
                VerifyErrorKind::HandlerCatchRegisterOutOfBounds {
                    handler: index,
                    register: handler.catch_register,
                    register_count: function.register_count,
                },
            ));
        }
    }
    // Deterministic O(n log n) laminar-family check over half-open ranges.
    // Sort handler indices by (start ascending, end descending) so an
    // enclosing range is always visited before any range it contains, then
    // sweep with a stack of open ancestor ends. `function.handlers` is never
    // mutated, so its original order (and the reported indices) is preserved.
    let mut order: Vec<usize> = (0..function.handlers.len()).collect();
    order.sort_by(|&left, &right| {
        let a = function.handlers[left];
        let b = function.handlers[right];
        a.start
            .get()
            .cmp(&b.start.get())
            .then_with(|| b.end.get().cmp(&a.end.get()))
    });
    let mut open: Vec<(u32, usize)> = Vec::new();
    for &index in &order {
        let range = function.handlers[index];
        let start = range.start.get();
        let end = range.end.get();
        // Disjoint and sibling ranges (including adjacency `[a, b) + [b, c)`)
        // close once the sweep passes their end.
        while open.last().is_some_and(|&(open_end, _)| open_end <= start) {
            open.pop();
        }
        if let Some(&(parent_end, parent_index)) = open.last() {
            // The current range shares an open ancestor. It must nest fully
            // inside it; extending past the ancestor's end is a partial overlap
            // (or a crossing), never proper nesting.
            if end > parent_end {
                let (left, right) = if parent_index < index {
                    (parent_index, index)
                } else {
                    (index, parent_index)
                };
                return Err(function_error(
                    function_index,
                    VerifyErrorKind::HandlersPartiallyOverlap { left, right },
                ));
            }
        }
        open.push((end, index));
    }
    Ok(())
}

fn verify_instruction(
    module: &Module<Unverified>,
    function_index: usize,
    function: &Function,
    pc: usize,
    instruction: Instruction,
) -> Result<(), VerifyError> {
    let register_count = function.register_count;
    let constant_count = module.constants.len();
    let function_count = module.functions.len();
    let code_len = function.code.len();

    let check_register = |register: Register| -> Result<(), VerifyError> {
        if register.get() >= register_count {
            Err(instruction_error(
                function_index,
                pc,
                VerifyErrorKind::RegisterOutOfBounds {
                    register,
                    register_count,
                },
            ))
        } else {
            Ok(())
        }
    };
    let check_constant = |constant: ConstantId| -> Result<(), VerifyError> {
        if constant.get() as usize >= constant_count {
            Err(instruction_error(
                function_index,
                pc,
                VerifyErrorKind::ConstantOutOfBounds {
                    constant,
                    constant_count,
                },
            ))
        } else {
            Ok(())
        }
    };
    let check_string_key = |constant: ConstantId| -> Result<(), VerifyError> {
        check_constant(constant)?;
        if matches!(
            module.constants[constant.get() as usize],
            Constant::String(_)
        ) {
            Ok(())
        } else {
            Err(instruction_error(
                function_index,
                pc,
                VerifyErrorKind::PropertyKeyNotString { constant },
            ))
        }
    };
    let check_window = |args_start: Register, arg_count: u32| -> Result<(), VerifyError> {
        if u64::from(args_start.get()) + u64::from(arg_count) > u64::from(register_count) {
            Err(instruction_error(
                function_index,
                pc,
                VerifyErrorKind::ArgumentWindowOutOfBounds {
                    args_start,
                    arg_count,
                    register_count,
                },
            ))
        } else {
            Ok(())
        }
    };

    match instruction {
        Instruction::LoadConst { dst, constant } => {
            check_register(dst)?;
            check_constant(constant)?;
        }
        Instruction::Move { dst, src } => {
            check_register(dst)?;
            check_register(src)?;
        }
        Instruction::Unary { dst, operand, .. } => {
            check_register(dst)?;
            check_register(operand)?;
        }
        Instruction::Binary {
            dst, left, right, ..
        } => {
            check_register(dst)?;
            check_register(left)?;
            check_register(right)?;
        }
        Instruction::CreateObject { dst } | Instruction::CreateArray { dst } => {
            check_register(dst)?;
        }
        Instruction::DefineFunction {
            dst,
            function: reference,
        } => {
            check_register(dst)?;
            if reference.get() as usize >= function_count {
                return Err(instruction_error(
                    function_index,
                    pc,
                    VerifyErrorKind::FunctionReferenceOutOfBounds {
                        function: reference,
                        function_count,
                    },
                ));
            }
        }
        Instruction::GetProperty { dst, object, key } => {
            check_register(dst)?;
            check_register(object)?;
            check_string_key(key)?;
        }
        Instruction::SetProperty { object, key, value } => {
            check_register(object)?;
            check_register(value)?;
            check_string_key(key)?;
        }
        Instruction::DeleteProperty { dst, object, key } => {
            check_register(dst)?;
            check_register(object)?;
            check_string_key(key)?;
        }
        Instruction::Call {
            dst,
            callee,
            this_value,
            args_start,
            arg_count,
        } => {
            check_register(dst)?;
            check_register(callee)?;
            check_register(this_value)?;
            check_window(args_start, arg_count)?;
        }
        Instruction::Construct {
            dst,
            callee,
            args_start,
            arg_count,
        } => {
            check_register(dst)?;
            check_register(callee)?;
            check_window(args_start, arg_count)?;
        }
        Instruction::Jump { target } => verify_target(function_index, pc, target, code_len)?,
        Instruction::JumpIfTrue { condition, target }
        | Instruction::JumpIfFalse { condition, target } => {
            check_register(condition)?;
            verify_target(function_index, pc, target, code_len)?;
        }
        Instruction::Return { value } | Instruction::Throw { value } => check_register(value)?,
        Instruction::Suspend { dst, src, resume } => {
            check_register(dst)?;
            check_register(src)?;
            verify_target(function_index, pc, resume, code_len)?;
        }
        Instruction::Import { dst, specifier } => {
            check_register(dst)?;
            check_string_key(specifier)?;
        }
        Instruction::Halt => {}
    }

    // Every normal successor (including fall-through) must be a real
    // instruction boundary: this forbids reachable fall-off past the end.
    let mut successor_error = None;
    instruction.visit_successors(pc as u32, |successor| {
        if successor_error.is_none() {
            successor_error = verify_target(function_index, pc, successor, code_len).err();
        }
    });
    if let Some(error) = successor_error {
        return Err(error);
    }
    Ok(())
}

fn verify_target(
    function_index: usize,
    pc: usize,
    target: Pc,
    instruction_count: usize,
) -> Result<(), VerifyError> {
    if target.get() as usize >= instruction_count {
        Err(instruction_error(
            function_index,
            pc,
            VerifyErrorKind::JumpOutOfBounds {
                target: target.get(),
                instruction_count,
            },
        ))
    } else {
        Ok(())
    }
}

/// Builds the greatest syntactic forward witness satisfying every transfer,
/// with the entry fact fixed to the parameter registers. Handler entries are
/// reached conservatively: an exception may occur at the first protected
/// instruction, so a handler's fact is the intersection of the pre-facts across
/// its protected range plus its catch register. This does not use semantic
/// reachability, matching Lean's `Certificate` and `verifier_never_skips_invariant`.
fn definite_initialization(
    function_index: usize,
    function: &Function,
) -> Result<Certificate, VerifyError> {
    let register_count = function.register_count;
    let mut facts = vec![RegisterSet::full(register_count); function.code.len()];
    let mut entry = RegisterSet::empty(register_count);
    entry.insert_prefix(function.parameter_count);
    facts[0] = entry;

    loop {
        let mut changed = false;
        for (pc, instruction) in function.code.iter().copied().enumerate() {
            let mut after = facts[pc].clone();
            if let Some(write) = instruction.write() {
                after.insert(write);
            }
            instruction.visit_successors(pc as u32, |successor| {
                changed |= facts[successor.get() as usize].intersect(&after);
            });
        }
        for handler in function.handlers.iter().copied() {
            let start = handler.start.get() as usize;
            let end = handler.end.get() as usize;
            let mut contribution = RegisterSet::full(register_count);
            for protected in &facts[start..end] {
                contribution.intersect(protected);
            }
            contribution.insert(handler.catch_register);
            changed |= facts[handler.handler.get() as usize].intersect(&contribution);
        }
        if !changed {
            break;
        }
    }

    for (pc, instruction) in function.code.iter().copied().enumerate() {
        let mut missing = None;
        instruction.visit_reads(|register| {
            if missing.is_none() && !facts[pc].contains(register) {
                missing = Some(register);
            }
        });
        if let Some(register) = missing {
            return Err(instruction_error(
                function_index,
                pc,
                VerifyErrorKind::ReadBeforeWrite { register },
            ));
        }
    }
    Ok(Certificate {
        register_count,
        facts,
    })
}

/// Decoder allocation/input ceilings, enforced before any allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    pub max_bytes: usize,
    pub max_constants: u32,
    pub max_functions: u32,
    pub max_parameter_count: u32,
    pub max_register_count: u32,
    pub max_instructions_per_function: u32,
    pub max_total_instructions: u64,
    pub max_handlers_per_function: u32,
    pub max_string_bytes: u32,
    pub max_arg_count: u32,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_constants: MAX_CONSTANTS,
            max_functions: MAX_FUNCTIONS,
            max_parameter_count: MAX_REGISTERS,
            max_register_count: MAX_REGISTERS,
            max_instructions_per_function: MAX_INSTRUCTIONS,
            max_total_instructions: 1 << 24,
            max_handlers_per_function: MAX_HANDLERS,
            max_string_bytes: 1 << 20,
            max_arg_count: MAX_REGISTERS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodeError {
    pub offset: usize,
    pub kind: DecodeErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeErrorKind {
    InputLimitExceeded {
        limit: usize,
        actual: usize,
    },
    UnexpectedEof,
    BadMagic {
        expected: u8,
        actual: u8,
    },
    UnsupportedVersion {
        version: u8,
    },
    MalformedInteger,
    NonCanonicalInteger,
    IntegerOverflow,
    InvalidConstantTag {
        tag: u8,
    },
    NonCanonicalNumber {
        bits: u64,
    },
    InvalidUtf8,
    InvalidBigInt,
    InvalidFunctionFlags {
        bits: u8,
    },
    InvalidUnaryOp {
        tag: u8,
    },
    InvalidBinaryOp {
        tag: u8,
    },
    InvalidOpcode {
        opcode: u8,
    },
    LimitExceeded {
        field: &'static str,
        limit: u64,
        actual: u64,
    },
    TrailingBytes {
        count: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "byte {}: ", self.offset)?;
        match self.kind {
            DecodeErrorKind::InputLimitExceeded { limit, actual } => {
                write!(formatter, "input has {actual} bytes, limit is {limit}")
            }
            DecodeErrorKind::UnexpectedEof => formatter.write_str("unexpected end of input"),
            DecodeErrorKind::BadMagic { expected, actual } => write!(
                formatter,
                "bad magic byte {actual:#04x}, expected {expected:#04x}"
            ),
            DecodeErrorKind::UnsupportedVersion { version } => {
                write!(formatter, "unsupported format version {version}")
            }
            DecodeErrorKind::MalformedInteger => formatter.write_str("malformed LEB128 integer"),
            DecodeErrorKind::NonCanonicalInteger => {
                formatter.write_str("noncanonical (overlong) LEB128 integer")
            }
            DecodeErrorKind::IntegerOverflow => {
                formatter.write_str("LEB128 integer exceeds 32 bits")
            }
            DecodeErrorKind::InvalidConstantTag { tag } => {
                write!(formatter, "invalid constant tag {tag}")
            }
            DecodeErrorKind::NonCanonicalNumber { bits } => {
                write!(formatter, "noncanonical NaN bits {bits:#018x}")
            }
            DecodeErrorKind::InvalidUtf8 => formatter.write_str("constant string is not UTF-8"),
            DecodeErrorKind::InvalidBigInt => {
                formatter.write_str("bigint constant is not canonical decimal text")
            }
            DecodeErrorKind::InvalidFunctionFlags { bits } => {
                write!(formatter, "invalid function flags {bits:#04x}")
            }
            DecodeErrorKind::InvalidUnaryOp { tag } => {
                write!(formatter, "invalid unary operator {tag}")
            }
            DecodeErrorKind::InvalidBinaryOp { tag } => {
                write!(formatter, "invalid binary operator {tag}")
            }
            DecodeErrorKind::InvalidOpcode { opcode } => {
                write!(formatter, "invalid opcode {opcode}")
            }
            DecodeErrorKind::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(formatter, "{field} value {actual} exceeds limit {limit}"),
            DecodeErrorKind::TrailingBytes { count } => write!(formatter, "{count} trailing bytes"),
        }
    }
}

impl Error for DecodeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    Decode(DecodeError),
    Verify(VerifyError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(formatter),
            Self::Verify(error) => error.fmt(formatter),
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Verify(error) => Some(error),
        }
    }
}

/// Strictly decodes untrusted bytes. Every length is checked before allocation;
/// semantic validity remains represented by the `Unverified` typestate.
///
/// # Errors
/// Returns the first malformed, noncanonical, or over-limit byte encountered,
/// or trailing bytes after a complete module.
pub fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<Module<Unverified>, DecodeError> {
    if bytes.len() > limits.max_bytes {
        return Err(DecodeError {
            offset: 0,
            kind: DecodeErrorKind::InputLimitExceeded {
                limit: limits.max_bytes,
                actual: bytes.len(),
            },
        });
    }
    let mut decoder = Decoder {
        bytes,
        offset: 0,
        limits,
        total_instructions: 0,
    };
    let module = decoder.module()?;
    if decoder.offset != bytes.len() {
        return Err(DecodeError {
            offset: decoder.offset,
            kind: DecodeErrorKind::TrailingBytes {
                count: bytes.len() - decoder.offset,
            },
        });
    }
    Ok(module)
}

/// Decodes and verifies in one boundary operation.
///
/// # Errors
/// Returns [`LoadError::Decode`] for malformed bytes or [`LoadError::Verify`]
/// for a structurally invalid module.
pub fn decode_verified(bytes: &[u8], limits: &DecodeLimits) -> Result<Module<Verified>, LoadError> {
    decode(bytes, limits)
        .map_err(LoadError::Decode)?
        .verify()
        .map_err(LoadError::Verify)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    limits: &'a DecodeLimits,
    total_instructions: u64,
}

impl<'a> Decoder<'a> {
    fn module(&mut self) -> Result<Module<Unverified>, DecodeError> {
        for expected in MAGIC {
            let at = self.offset;
            let actual = self.byte()?;
            if actual != expected {
                return Err(self.error(at, DecodeErrorKind::BadMagic { expected, actual }));
            }
        }
        let version_at = self.offset;
        let version = self.byte()?;
        if version != FORMAT_VERSION {
            return Err(self.error(version_at, DecodeErrorKind::UnsupportedVersion { version }));
        }

        let constant_count = self.bounded("constant count", self.limits.max_constants)?;
        let mut constants = Vec::with_capacity(self.cap(constant_count));
        for _ in 0..constant_count {
            constants.push(self.constant()?);
        }

        let function_count = self.bounded("function count", self.limits.max_functions)?;
        let entry = FunctionId::new(self.leb128()?);
        let mut functions = Vec::with_capacity(self.cap(function_count));
        for _ in 0..function_count {
            functions.push(self.function()?);
        }
        Ok(Module::new(constants, functions, entry))
    }

    fn constant(&mut self) -> Result<Constant, DecodeError> {
        let tag_at = self.offset;
        match self.byte()? {
            0 => {
                let number_at = self.offset;
                let bits = u64::from_le_bytes(self.exact::<8>()?);
                NumberBits::from_wire(bits)
                    .map(Constant::Number)
                    .ok_or_else(|| {
                        self.error(number_at, DecodeErrorKind::NonCanonicalNumber { bits })
                    })
            }
            1 => Ok(Constant::Int32(i32::from_le_bytes(self.exact::<4>()?))),
            2 => Ok(Constant::String(self.text()?)),
            3 => Ok(Constant::Boolean(false)),
            4 => Ok(Constant::Boolean(true)),
            5 => Ok(Constant::Null),
            6 => Ok(Constant::Undefined),
            7 => {
                let start = self.offset;
                let text = self.text()?;
                BigIntLiteral::new(text)
                    .map(Constant::BigInt)
                    .ok_or_else(|| self.error(start, DecodeErrorKind::InvalidBigInt))
            }
            tag => Err(self.error(tag_at, DecodeErrorKind::InvalidConstantTag { tag })),
        }
    }

    fn text(&mut self) -> Result<String, DecodeError> {
        let length = self.bounded("string byte length", self.limits.max_string_bytes)?;
        let start = self.offset;
        let bytes = self.slice(length as usize)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| self.error(start, DecodeErrorKind::InvalidUtf8))?;
        Ok(value.to_owned())
    }

    fn function(&mut self) -> Result<Function, DecodeError> {
        let name = match self.leb128()? {
            0 => None,
            encoded => Some(ConstantId::new(encoded - 1)),
        };
        let parameter_count = self.bounded("parameter count", self.limits.max_parameter_count)?;
        let register_count = self.bounded("register count", self.limits.max_register_count)?;
        let flags_at = self.offset;
        let flags_bits = self.byte()?;
        let flags = FunctionFlags::from_bits(flags_bits).ok_or_else(|| {
            self.error(
                flags_at,
                DecodeErrorKind::InvalidFunctionFlags { bits: flags_bits },
            )
        })?;
        let instruction_at = self.offset;
        let instruction_count = self.bounded(
            "instruction count",
            self.limits.max_instructions_per_function,
        )?;
        self.total_instructions += u64::from(instruction_count);
        if self.total_instructions > self.limits.max_total_instructions {
            return Err(self.error(
                instruction_at,
                DecodeErrorKind::LimitExceeded {
                    field: "total instruction count",
                    limit: self.limits.max_total_instructions,
                    actual: self.total_instructions,
                },
            ));
        }
        let mut code = Vec::with_capacity(self.cap(instruction_count));
        for _ in 0..instruction_count {
            code.push(self.instruction()?);
        }

        let handler_count = self.bounded("handler count", self.limits.max_handlers_per_function)?;
        let mut handlers = Vec::with_capacity(self.cap(handler_count));
        for _ in 0..handler_count {
            handlers.push(ExceptionHandler {
                start: Pc::new(self.leb128()?),
                end: Pc::new(self.leb128()?),
                handler: Pc::new(self.leb128()?),
                catch_register: Register::new(self.leb128()?),
            });
        }
        Ok(Function::new(
            name,
            parameter_count,
            register_count,
            flags,
            code,
            handlers,
        ))
    }

    fn instruction(&mut self) -> Result<Instruction, DecodeError> {
        let opcode_at = self.offset;
        match self.byte()? {
            0 => Ok(Instruction::LoadConst {
                dst: Register::new(self.leb128()?),
                constant: ConstantId::new(self.leb128()?),
            }),
            1 => Ok(Instruction::Move {
                dst: Register::new(self.leb128()?),
                src: Register::new(self.leb128()?),
            }),
            2 => Ok(Instruction::Unary {
                dst: Register::new(self.leb128()?),
                op: self.unary_op()?,
                operand: Register::new(self.leb128()?),
            }),
            3 => Ok(Instruction::Binary {
                dst: Register::new(self.leb128()?),
                op: self.binary_op()?,
                left: Register::new(self.leb128()?),
                right: Register::new(self.leb128()?),
            }),
            4 => Ok(Instruction::CreateObject {
                dst: Register::new(self.leb128()?),
            }),
            5 => Ok(Instruction::CreateArray {
                dst: Register::new(self.leb128()?),
            }),
            6 => Ok(Instruction::DefineFunction {
                dst: Register::new(self.leb128()?),
                function: FunctionId::new(self.leb128()?),
            }),
            7 => Ok(Instruction::GetProperty {
                dst: Register::new(self.leb128()?),
                object: Register::new(self.leb128()?),
                key: ConstantId::new(self.leb128()?),
            }),
            8 => Ok(Instruction::SetProperty {
                object: Register::new(self.leb128()?),
                key: ConstantId::new(self.leb128()?),
                value: Register::new(self.leb128()?),
            }),
            9 => Ok(Instruction::DeleteProperty {
                dst: Register::new(self.leb128()?),
                object: Register::new(self.leb128()?),
                key: ConstantId::new(self.leb128()?),
            }),
            10 => Ok(Instruction::Call {
                dst: Register::new(self.leb128()?),
                callee: Register::new(self.leb128()?),
                this_value: Register::new(self.leb128()?),
                args_start: Register::new(self.leb128()?),
                arg_count: self.bounded("argument count", self.limits.max_arg_count)?,
            }),
            11 => Ok(Instruction::Construct {
                dst: Register::new(self.leb128()?),
                callee: Register::new(self.leb128()?),
                args_start: Register::new(self.leb128()?),
                arg_count: self.bounded("argument count", self.limits.max_arg_count)?,
            }),
            12 => Ok(Instruction::Jump {
                target: Pc::new(self.leb128()?),
            }),
            13 => Ok(Instruction::JumpIfTrue {
                condition: Register::new(self.leb128()?),
                target: Pc::new(self.leb128()?),
            }),
            14 => Ok(Instruction::JumpIfFalse {
                condition: Register::new(self.leb128()?),
                target: Pc::new(self.leb128()?),
            }),
            15 => Ok(Instruction::Return {
                value: Register::new(self.leb128()?),
            }),
            16 => Ok(Instruction::Throw {
                value: Register::new(self.leb128()?),
            }),
            17 => Ok(Instruction::Suspend {
                dst: Register::new(self.leb128()?),
                src: Register::new(self.leb128()?),
                resume: Pc::new(self.leb128()?),
            }),
            18 => Ok(Instruction::Import {
                dst: Register::new(self.leb128()?),
                specifier: ConstantId::new(self.leb128()?),
            }),
            19 => Ok(Instruction::Halt),
            opcode => Err(self.error(opcode_at, DecodeErrorKind::InvalidOpcode { opcode })),
        }
    }

    fn unary_op(&mut self) -> Result<UnaryOp, DecodeError> {
        let at = self.offset;
        let tag = self.byte()?;
        UnaryOp::from_u8(tag).ok_or_else(|| self.error(at, DecodeErrorKind::InvalidUnaryOp { tag }))
    }

    fn binary_op(&mut self) -> Result<BinaryOp, DecodeError> {
        let at = self.offset;
        let tag = self.byte()?;
        BinaryOp::from_u8(tag)
            .ok_or_else(|| self.error(at, DecodeErrorKind::InvalidBinaryOp { tag }))
    }

    fn bounded(&mut self, field: &'static str, limit: u32) -> Result<u32, DecodeError> {
        let at = self.offset;
        let actual = self.leb128()?;
        if actual > limit {
            Err(self.error(
                at,
                DecodeErrorKind::LimitExceeded {
                    field,
                    limit: u64::from(limit),
                    actual: u64::from(actual),
                },
            ))
        } else {
            Ok(actual)
        }
    }

    /// Reads one canonical unsigned LEB128 `u32`. Rejects EOF mid-integer,
    /// overlong (trailing-zero) encodings, and values exceeding 32 bits.
    fn leb128(&mut self) -> Result<u32, DecodeError> {
        let start = self.offset;
        let mut result: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.byte()?;
            if shift == 28 {
                // Fifth group: only the low four bits may be set, and the
                // continuation bit must be clear (else overflow); a zero final
                // group would be overlong.
                if byte & 0x80 != 0 || byte > 0x0f {
                    return Err(self.error(start, DecodeErrorKind::IntegerOverflow));
                }
                if byte == 0 {
                    return Err(self.error(start, DecodeErrorKind::NonCanonicalInteger));
                }
                return Ok(result | (u32::from(byte) << 28));
            }
            result |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && self.offset - start > 1 {
                    return Err(self.error(start, DecodeErrorKind::NonCanonicalInteger));
                }
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn cap(&self, count: u32) -> usize {
        // Each element consumes at least one wire byte, so the remaining input
        // caps how many can actually be present; never pre-allocate beyond it.
        (count as usize).min(self.bytes.len().saturating_sub(self.offset))
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        let Some(byte) = self.bytes.get(self.offset).copied() else {
            return Err(self.error(self.offset, DecodeErrorKind::UnexpectedEof));
        };
        self.offset += 1;
        Ok(byte)
    }

    fn exact<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self.slice(N)?;
        let mut result = [0; N];
        result.copy_from_slice(bytes);
        Ok(result)
    }

    fn slice(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let source: &'a [u8] = self.bytes;
        let Some(end) = self.offset.checked_add(length) else {
            return Err(self.error(self.offset, DecodeErrorKind::UnexpectedEof));
        };
        let Some(bytes) = source.get(self.offset..end) else {
            return Err(self.error(source.len(), DecodeErrorKind::UnexpectedEof));
        };
        self.offset = end;
        Ok(bytes)
    }

    const fn error(&self, offset: usize, kind: DecodeErrorKind) -> DecodeError {
        DecodeError { offset, kind }
    }
}

fn write_u32(value: u32, output: &mut Vec<u8>) {
    let mut remaining = value;
    loop {
        let byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

fn encode_constant(constant: &Constant, output: &mut Vec<u8>) {
    match constant {
        Constant::Number(bits) => {
            output.push(0);
            output.extend_from_slice(&bits.bits().to_le_bytes());
        }
        Constant::Int32(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_le_bytes());
        }
        Constant::String(value) => {
            output.push(2);
            write_text(value, output);
        }
        Constant::Boolean(false) => output.push(3),
        Constant::Boolean(true) => output.push(4),
        Constant::Null => output.push(5),
        Constant::Undefined => output.push(6),
        Constant::BigInt(value) => {
            output.push(7);
            write_text(value.as_str(), output);
        }
    }
}

fn write_text(value: &str, output: &mut Vec<u8>) {
    write_u32(value.len() as u32, output);
    output.extend_from_slice(value.as_bytes());
}

fn encode_function(function: &Function, output: &mut Vec<u8>) {
    write_u32(function.name.map_or(0, |name| name.get() + 1), output);
    write_u32(function.parameter_count, output);
    write_u32(function.register_count, output);
    output.push(function.flags.to_bits());
    write_u32(function.code.len() as u32, output);
    for instruction in &function.code {
        encode_instruction(*instruction, output);
    }
    write_u32(function.handlers.len() as u32, output);
    for handler in &function.handlers {
        write_u32(handler.start.get(), output);
        write_u32(handler.end.get(), output);
        write_u32(handler.handler.get(), output);
        write_u32(handler.catch_register.get(), output);
    }
}

fn encode_instruction(instruction: Instruction, output: &mut Vec<u8>) {
    match instruction {
        Instruction::LoadConst { dst, constant } => {
            output.push(0);
            write_u32(dst.get(), output);
            write_u32(constant.get(), output);
        }
        Instruction::Move { dst, src } => {
            output.push(1);
            write_u32(dst.get(), output);
            write_u32(src.get(), output);
        }
        Instruction::Unary { dst, op, operand } => {
            output.push(2);
            write_u32(dst.get(), output);
            output.push(op.to_u8());
            write_u32(operand.get(), output);
        }
        Instruction::Binary {
            dst,
            op,
            left,
            right,
        } => {
            output.push(3);
            write_u32(dst.get(), output);
            output.push(op.to_u8());
            write_u32(left.get(), output);
            write_u32(right.get(), output);
        }
        Instruction::CreateObject { dst } => {
            output.push(4);
            write_u32(dst.get(), output);
        }
        Instruction::CreateArray { dst } => {
            output.push(5);
            write_u32(dst.get(), output);
        }
        Instruction::DefineFunction { dst, function } => {
            output.push(6);
            write_u32(dst.get(), output);
            write_u32(function.get(), output);
        }
        Instruction::GetProperty { dst, object, key } => {
            output.push(7);
            write_u32(dst.get(), output);
            write_u32(object.get(), output);
            write_u32(key.get(), output);
        }
        Instruction::SetProperty { object, key, value } => {
            output.push(8);
            write_u32(object.get(), output);
            write_u32(key.get(), output);
            write_u32(value.get(), output);
        }
        Instruction::DeleteProperty { dst, object, key } => {
            output.push(9);
            write_u32(dst.get(), output);
            write_u32(object.get(), output);
            write_u32(key.get(), output);
        }
        Instruction::Call {
            dst,
            callee,
            this_value,
            args_start,
            arg_count,
        } => {
            output.push(10);
            write_u32(dst.get(), output);
            write_u32(callee.get(), output);
            write_u32(this_value.get(), output);
            write_u32(args_start.get(), output);
            write_u32(arg_count, output);
        }
        Instruction::Construct {
            dst,
            callee,
            args_start,
            arg_count,
        } => {
            output.push(11);
            write_u32(dst.get(), output);
            write_u32(callee.get(), output);
            write_u32(args_start.get(), output);
            write_u32(arg_count, output);
        }
        Instruction::Jump { target } => {
            output.push(12);
            write_u32(target.get(), output);
        }
        Instruction::JumpIfTrue { condition, target } => {
            output.push(13);
            write_u32(condition.get(), output);
            write_u32(target.get(), output);
        }
        Instruction::JumpIfFalse { condition, target } => {
            output.push(14);
            write_u32(condition.get(), output);
            write_u32(target.get(), output);
        }
        Instruction::Return { value } => {
            output.push(15);
            write_u32(value.get(), output);
        }
        Instruction::Throw { value } => {
            output.push(16);
            write_u32(value.get(), output);
        }
        Instruction::Suspend { dst, src, resume } => {
            output.push(17);
            write_u32(dst.get(), output);
            write_u32(src.get(), output);
            write_u32(resume.get(), output);
        }
        Instruction::Import { dst, specifier } => {
            output.push(18);
            write_u32(dst.get(), output);
            write_u32(specifier.get(), output);
        }
        Instruction::Halt => output.push(19),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> FunctionFlags {
        FunctionFlags::default()
    }

    fn prefix() -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION);
        bytes
    }

    /// A function exercising most opcodes, all references in bounds and every
    /// read dominated by a write.
    fn rich_module() -> Module<Unverified> {
        let constants = vec![
            Constant::String("main".to_owned()), // 0: function name + key
            Constant::Int32(21),                 // 1
            Constant::Number(NumberBits::from_f64(1.5)), // 2
            Constant::BigInt(BigIntLiteral::new("-1234567890123".to_owned()).unwrap()), // 3
            Constant::Boolean(true),             // 4
            Constant::Null,                      // 5
            Constant::Undefined,                 // 6
            Constant::String("./dep".to_owned()), // 7: import specifier
        ];
        let code = vec![
            Instruction::LoadConst {
                dst: Register::new(0),
                constant: ConstantId::new(1),
            },
            Instruction::LoadConst {
                dst: Register::new(1),
                constant: ConstantId::new(2),
            },
            Instruction::Binary {
                dst: Register::new(2),
                op: BinaryOp::Add,
                left: Register::new(0),
                right: Register::new(1),
            },
            Instruction::Unary {
                dst: Register::new(3),
                op: UnaryOp::Negate,
                operand: Register::new(2),
            },
            Instruction::CreateObject {
                dst: Register::new(4),
            },
            Instruction::SetProperty {
                object: Register::new(4),
                key: ConstantId::new(0),
                value: Register::new(3),
            },
            Instruction::GetProperty {
                dst: Register::new(5),
                object: Register::new(4),
                key: ConstantId::new(0),
            },
            Instruction::CreateArray {
                dst: Register::new(6),
            },
            Instruction::DefineFunction {
                dst: Register::new(7),
                function: FunctionId::new(0),
            },
            Instruction::Move {
                dst: Register::new(8),
                src: Register::new(5),
            },
            Instruction::Call {
                dst: Register::new(9),
                callee: Register::new(7),
                this_value: Register::new(4),
                args_start: Register::new(8),
                arg_count: 1,
            },
            Instruction::Construct {
                dst: Register::new(10),
                callee: Register::new(7),
                args_start: Register::new(8),
                arg_count: 1,
            },
            Instruction::Import {
                dst: Register::new(11),
                specifier: ConstantId::new(7),
            },
            Instruction::Suspend {
                dst: Register::new(12),
                src: Register::new(10),
                resume: Pc::new(14),
            },
            Instruction::JumpIfTrue {
                condition: Register::new(9),
                target: Pc::new(16),
            },
            Instruction::DeleteProperty {
                dst: Register::new(13),
                object: Register::new(4),
                key: ConstantId::new(0),
            },
            Instruction::Return {
                value: Register::new(9),
            },
            Instruction::Return {
                value: Register::new(13),
            },
        ];
        let handlers = vec![ExceptionHandler {
            start: Pc::new(0),
            end: Pc::new(16),
            handler: Pc::new(17),
            catch_register: Register::new(13),
        }];
        Module::new(
            constants,
            vec![Function::new(
                Some(ConstantId::new(0)),
                0,
                14,
                FunctionFlags {
                    is_async: true,
                    is_generator: false,
                },
                code,
                handlers,
            )],
            FunctionId::new(0),
        )
    }

    #[test]
    fn rich_module_verifies_round_trips_and_is_deterministic() {
        let verified = rich_module().verify().expect("valid production module");
        let encoded = verified.encode();
        assert_eq!(verified.encode(), encoded, "encoding is deterministic");

        let decoded = decode(&encoded, &DecodeLimits::default()).expect("canonical wire");
        assert_eq!(decoded, rich_module(), "decode is the inverse of encode");
        let reverified = decoded.verify().expect("decoded module reverifies");
        assert_eq!(reverified.encode(), encoded, "round-trip is canonical");
    }

    #[test]
    fn minimal_module_has_exact_canonical_wire() {
        let module = Module::new(
            vec![Constant::Int32(7)],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![
                    Instruction::LoadConst {
                        dst: Register::new(0),
                        constant: ConstantId::new(0),
                    },
                    Instruction::Return {
                        value: Register::new(0),
                    },
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        );
        let encoded = module.verify().expect("valid").encode();
        let mut expected = prefix();
        expected.extend_from_slice(&[
            1, // constant count
            1, 7, 0, 0, 0, // Int32(7)
            1, // function count
            0, // entry
            0, // name none
            0, // parameter count
            1, // register count
            0, // flags
            2, // code length
            0, 0, 0, // LoadConst dst0 const0
            15, 0, // Return value0
            0, // handler count
        ]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn fields_beyond_127_scale_and_round_trip() {
        // 200 constants, 200 registers, 200 instructions -> multi-byte LEB128.
        let constant_count: u32 = 200;
        let mut constants = Vec::new();
        for value in 0..constant_count {
            constants.push(Constant::Int32(value as i32));
        }
        let register_count: u32 = 200;
        let mut code = Vec::new();
        for register in 0..register_count {
            code.push(Instruction::LoadConst {
                dst: Register::new(register),
                constant: ConstantId::new(register % constant_count),
            });
        }
        code.push(Instruction::Return {
            value: Register::new(register_count - 1),
        });
        let module = Module::new(
            constants,
            vec![Function::new(
                None,
                0,
                register_count,
                flags(),
                code,
                Vec::new(),
            )],
            FunctionId::new(0),
        );
        let verified = module.clone().verify().expect("large module verifies");
        let encoded = verified.encode();
        // 200 needs two LEB128 bytes; confirm the multi-byte path is exercised.
        assert!(
            (0..encoded.len().saturating_sub(1))
                .any(|i| encoded[i] == 0xc8 && encoded[i + 1] == 0x01),
            "constant count 200 must use a two-byte LEB128 group"
        );
        let decoded = decode(&encoded, &DecodeLimits::default()).expect("round trip");
        assert_eq!(decoded, module);
        assert_eq!(decoded.verify().expect("reverify").encode(), encoded,);
    }

    fn decode_leb(bytes: &[u8]) -> Result<u32, DecodeError> {
        let limits = DecodeLimits::default();
        let mut decoder = Decoder {
            bytes,
            offset: 0,
            limits: &limits,
            total_instructions: 0,
        };
        decoder.leb128()
    }

    #[test]
    fn leb128_accepts_only_canonical_encodings() {
        assert_eq!(decode_leb(&[0]), Ok(0));
        assert_eq!(decode_leb(&[1]), Ok(1));
        assert_eq!(decode_leb(&[127]), Ok(127));
        assert_eq!(decode_leb(&[0xc8, 0x01]), Ok(200));
        assert_eq!(decode_leb(&[0xff, 0xff, 0xff, 0xff, 0x0f]), Ok(u32::MAX));

        // Overlong single zero group.
        assert_eq!(
            decode_leb(&[0x80, 0x00]),
            Err(DecodeError {
                offset: 0,
                kind: DecodeErrorKind::NonCanonicalInteger,
            })
        );
        // Overlong nonzero value.
        assert_eq!(
            decode_leb(&[0x81, 0x00]),
            Err(DecodeError {
                offset: 0,
                kind: DecodeErrorKind::NonCanonicalInteger,
            })
        );
        // Truncated mid-integer: EOF is reported where the missing
        // continuation byte belongs (offset 1, after consuming 0x80).
        assert_eq!(
            decode_leb(&[0x80]),
            Err(DecodeError {
                offset: 1,
                kind: DecodeErrorKind::UnexpectedEof,
            })
        );
        // Overflow beyond 32 bits (sixth group / high bits on fifth group).
        assert_eq!(
            decode_leb(&[0xff, 0xff, 0xff, 0xff, 0x1f]),
            Err(DecodeError {
                offset: 0,
                kind: DecodeErrorKind::IntegerOverflow,
            })
        );
        assert_eq!(
            decode_leb(&[0x80, 0x80, 0x80, 0x80, 0x80]),
            Err(DecodeError {
                offset: 0,
                kind: DecodeErrorKind::IntegerOverflow,
            })
        );
    }

    #[test]
    fn decode_errors_report_the_first_bad_byte() {
        let mut bytes = prefix();
        bytes[3] ^= 1;
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()),
            Err(DecodeError {
                offset: 3,
                kind: DecodeErrorKind::BadMagic {
                    expected: MAGIC[3],
                    actual: bytes[3],
                },
            })
        );
        assert_eq!(
            decode(&MAGIC[..4], &DecodeLimits::default()),
            Err(DecodeError {
                offset: 4,
                kind: DecodeErrorKind::UnexpectedEof,
            })
        );
        let mut bad_version = MAGIC.to_vec();
        bad_version.push(2);
        assert_eq!(
            decode(&bad_version, &DecodeLimits::default()),
            Err(DecodeError {
                offset: 8,
                kind: DecodeErrorKind::UnsupportedVersion { version: 2 },
            })
        );
    }

    #[test]
    fn decoder_checks_limits_before_allocation() {
        let mut bytes = prefix();
        write_u32(200, &mut bytes); // constant count, two-byte LEB128 [0xc8, 0x01]
        let limits = DecodeLimits {
            max_constants: 128,
            ..DecodeLimits::default()
        };
        assert_eq!(
            decode(&bytes, &limits),
            Err(DecodeError {
                offset: prefix().len(),
                kind: DecodeErrorKind::LimitExceeded {
                    field: "constant count",
                    limit: 128,
                    actual: 200,
                },
            })
        );
    }

    #[test]
    fn decoder_rejects_invalid_utf8_without_recovery() {
        let mut bytes = prefix();
        write_u32(1, &mut bytes); // constant count
        bytes.push(2); // string tag
        write_u32(1, &mut bytes); // length
        bytes.push(0xff); // invalid UTF-8
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()),
            Err(DecodeError {
                offset: prefix().len() + 3,
                kind: DecodeErrorKind::InvalidUtf8,
            })
        );
    }

    #[test]
    fn decoder_rejects_noncanonical_nan_bits() {
        let mut bytes = prefix();
        write_u32(1, &mut bytes);
        bytes.push(0); // number tag
        bytes.extend_from_slice(&0x7ff0_0000_0000_0001_u64.to_le_bytes());
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()),
            Err(DecodeError {
                offset: prefix().len() + 2,
                kind: DecodeErrorKind::NonCanonicalNumber {
                    bits: 0x7ff0_0000_0000_0001,
                },
            })
        );
        assert_eq!(
            NumberBits::from_bits(0xfff0_0000_0000_0001).bits(),
            CANONICAL_NAN_BITS
        );
    }

    #[test]
    fn decoder_rejects_invalid_bigint_text() {
        for text in ["007", "-0", "", "-", "1a", "+1", "00"] {
            let mut bytes = prefix();
            write_u32(1, &mut bytes);
            bytes.push(7); // bigint tag
            write_text(text, &mut bytes);
            assert!(
                matches!(
                    decode(&bytes, &DecodeLimits::default()),
                    Err(DecodeError {
                        kind: DecodeErrorKind::InvalidBigInt,
                        ..
                    })
                ),
                "expected {text:?} to be rejected"
            );
        }
        // Canonical forms accepted.
        for text in ["0", "7", "-7", "1234567890123456789012345"] {
            assert!(BigIntLiteral::new(text.to_owned()).is_some(), "{text}");
        }
    }

    #[test]
    fn decoder_rejects_unknown_tags_flags_and_operators() {
        // Invalid constant tag.
        let mut bad_constant = prefix();
        write_u32(1, &mut bad_constant);
        bad_constant.push(200);
        assert!(matches!(
            decode(&bad_constant, &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::InvalidConstantTag { tag: 200 },
                ..
            })
        ));

        // Invalid function flags (unknown bit).
        let mut bad_flags = prefix();
        write_u32(0, &mut bad_flags); // constants
        write_u32(1, &mut bad_flags); // functions
        write_u32(0, &mut bad_flags); // entry
        write_u32(0, &mut bad_flags); // name none
        write_u32(0, &mut bad_flags); // params
        write_u32(1, &mut bad_flags); // registers
        bad_flags.push(0b100); // unknown flag bit
        assert!(matches!(
            decode(&bad_flags, &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::InvalidFunctionFlags { bits: 0b100 },
                ..
            })
        ));

        // Invalid opcode and invalid operator.
        let base = |body: &[u8]| {
            let mut bytes = prefix();
            write_u32(0, &mut bytes); // constants
            write_u32(1, &mut bytes); // functions
            write_u32(0, &mut bytes); // entry
            write_u32(0, &mut bytes); // name
            write_u32(0, &mut bytes); // params
            write_u32(1, &mut bytes); // registers
            bytes.push(0); // flags
            write_u32(1, &mut bytes); // code length
            bytes.extend_from_slice(body);
            bytes
        };
        assert!(matches!(
            decode(&base(&[250]), &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::InvalidOpcode { opcode: 250 },
                ..
            })
        ));
        // Unary with a bad operator tag (opcode 2, dst 0, op 99, ...).
        assert!(matches!(
            decode(&base(&[2, 0, 99, 0]), &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::InvalidUnaryOp { tag: 99 },
                ..
            })
        ));
        // Binary with a bad operator tag (opcode 3, dst 0, op 99, ...).
        assert!(matches!(
            decode(&base(&[3, 0, 99, 0, 0]), &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::InvalidBinaryOp { tag: 99 },
                ..
            })
        ));
    }

    #[test]
    fn decoder_rejects_trailing_bytes() {
        let mut trailing = rich_module().verify().expect("valid").encode();
        trailing.push(0);
        assert!(matches!(
            decode(&trailing, &DecodeLimits::default()),
            Err(DecodeError {
                kind: DecodeErrorKind::TrailingBytes { count: 1 },
                ..
            })
        ));
    }

    #[test]
    fn decode_is_total_over_hostile_bytes() {
        // Every truncation of a valid module either decodes or errors, never
        // panics; and adversarial byte soup never panics.
        let valid = rich_module().verify().expect("valid").encode();
        for len in 0..=valid.len() {
            let _ = decode(&valid[..len], &DecodeLimits::default());
        }
        for seed in 0u16..=255 {
            let soup: Vec<u8> = (0..64).map(|i| (seed as u8).wrapping_mul(i + 1)).collect();
            let _ = decode(&soup, &DecodeLimits::default());
        }
    }

    #[test]
    fn single_byte_mutations_change_decode_result() {
        let encoded = rich_module().verify().expect("valid").encode();
        let baseline = decode(&encoded, &DecodeLimits::default());
        let mut observed_difference = false;
        for index in 0..encoded.len() {
            let mut mutated = encoded.clone();
            mutated[index] = mutated[index].wrapping_add(1);
            if decode(&mutated, &DecodeLimits::default()) != baseline {
                observed_difference = true;
            }
        }
        assert!(
            observed_difference,
            "flipping bytes must be observable at the decode boundary"
        );
    }

    #[test]
    fn verifier_rejects_empty_module_and_bad_entry() {
        assert_eq!(
            Module::new(vec![], vec![], FunctionId::new(0)).verify(),
            Err(module_error(VerifyErrorKind::EmptyModule))
        );
        let bad_entry = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                0,
                flags(),
                vec![Instruction::Halt],
                vec![],
            )],
            FunctionId::new(1),
        );
        assert!(matches!(
            bad_entry.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::EntryFunctionOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_bad_function_metadata() {
        let empty_function = Module::new(
            vec![],
            vec![Function::new(None, 0, 0, flags(), vec![], vec![])],
            FunctionId::new(0),
        );
        assert!(matches!(
            empty_function.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::EmptyFunction,
                ..
            })
        ));

        let too_many_params = Module::new(
            vec![],
            vec![Function::new(
                None,
                2,
                1,
                flags(),
                vec![Instruction::Halt],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            too_many_params.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::ParameterCountExceedsRegisters { .. },
                ..
            })
        ));

        let bad_name = Module::new(
            vec![Constant::Int32(0)],
            vec![Function::new(
                Some(ConstantId::new(0)),
                0,
                0,
                flags(),
                vec![Instruction::Halt],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_name.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::FunctionNameNotString { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_out_of_bounds_references() {
        let bad_register = Module::new(
            vec![Constant::Int32(0)],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::LoadConst {
                    dst: Register::new(1),
                    constant: ConstantId::new(0),
                }],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_register.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::RegisterOutOfBounds { .. },
                ..
            })
        ));

        let bad_constant = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::LoadConst {
                    dst: Register::new(0),
                    constant: ConstantId::new(0),
                }],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_constant.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::ConstantOutOfBounds { .. },
                ..
            })
        ));

        let non_string_key = Module::new(
            vec![Constant::Int32(0)],
            vec![Function::new(
                None,
                0,
                2,
                flags(),
                vec![
                    Instruction::CreateObject {
                        dst: Register::new(0),
                    },
                    Instruction::GetProperty {
                        dst: Register::new(1),
                        object: Register::new(0),
                        key: ConstantId::new(0),
                    },
                    Instruction::Return {
                        value: Register::new(1),
                    },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            non_string_key.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::PropertyKeyNotString { .. },
                ..
            })
        ));

        let bad_function_ref = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![
                    Instruction::DefineFunction {
                        dst: Register::new(0),
                        function: FunctionId::new(5),
                    },
                    Instruction::Return {
                        value: Register::new(0),
                    },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_function_ref.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::FunctionReferenceOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_argument_window_escape() {
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                2,
                flags(),
                vec![
                    Instruction::CreateObject {
                        dst: Register::new(0),
                    },
                    Instruction::CreateObject {
                        dst: Register::new(1),
                    },
                    Instruction::Call {
                        dst: Register::new(0),
                        callee: Register::new(1),
                        this_value: Register::new(0),
                        args_start: Register::new(1),
                        arg_count: 5, // 1 + 5 = 6 > register_count 2
                    },
                    Instruction::Return {
                        value: Register::new(0),
                    },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::ArgumentWindowOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_reachable_falloff() {
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                // Last instruction is a non-terminator: falls off the end.
                vec![Instruction::CreateObject {
                    dst: Register::new(0),
                }],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::JumpOutOfBounds { target: 1, .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_bad_jump_and_conditional_targets() {
        for instruction in [
            Instruction::Jump { target: Pc::new(9) },
            Instruction::JumpIfFalse {
                condition: Register::new(0),
                target: Pc::new(9),
            },
        ] {
            let module = Module::new(
                vec![],
                vec![Function::new(
                    None,
                    1, // register 0 is a parameter so the condition read is valid
                    1,
                    flags(),
                    vec![instruction],
                    vec![],
                )],
                FunctionId::new(0),
            );
            assert!(matches!(
                module.verify(),
                Err(VerifyError {
                    kind: VerifyErrorKind::JumpOutOfBounds { .. },
                    ..
                })
            ));
        }
    }

    #[test]
    fn verifier_rejects_read_before_write() {
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                3,
                flags(),
                vec![
                    Instruction::Binary {
                        dst: Register::new(2),
                        op: BinaryOp::Add,
                        left: Register::new(0),
                        right: Register::new(1),
                    },
                    Instruction::Return {
                        value: Register::new(2),
                    },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert_eq!(
            module.verify(),
            Err(VerifyError {
                function: Some(FunctionId::new(0)),
                instruction: Some(Pc::new(0)),
                kind: VerifyErrorKind::ReadBeforeWrite {
                    register: Register::new(0),
                },
            })
        );
    }

    #[test]
    fn parameters_are_initialized_on_entry() {
        // Two parameters read immediately with no preceding write must verify.
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                2,
                3,
                flags(),
                vec![
                    Instruction::Binary {
                        dst: Register::new(2),
                        op: BinaryOp::Add,
                        left: Register::new(0),
                        right: Register::new(1),
                    },
                    Instruction::Return {
                        value: Register::new(2),
                    },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        let verified = module.verify().expect("parameters count as initialized");
        let certificate = verified.certificate(FunctionId::new(0)).unwrap();
        assert_eq!(
            certificate.initialized_before(Pc::new(0), Register::new(0)),
            Some(true)
        );
        assert_eq!(
            certificate.initialized_before(Pc::new(0), Register::new(2)),
            Some(false)
        );
    }

    #[test]
    fn catch_register_is_observable_in_handler() {
        // The handler reads its catch register with no in-block write; this is
        // sound only because handler dispatch initializes the catch register.
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![
                    Instruction::CreateObject {
                        dst: Register::new(0),
                    },
                    Instruction::Throw {
                        value: Register::new(0),
                    },
                    Instruction::Return {
                        value: Register::new(0),
                    }, // handler
                ],
                vec![ExceptionHandler {
                    start: Pc::new(0),
                    end: Pc::new(2),
                    handler: Pc::new(2),
                    catch_register: Register::new(0),
                }],
            )],
            FunctionId::new(0),
        );
        let verified = module
            .verify()
            .expect("catch register initialized on dispatch");
        let certificate = verified.certificate(FunctionId::new(0)).unwrap();
        assert_eq!(
            certificate.initialized_before(Pc::new(2), Register::new(0)),
            Some(true)
        );
    }

    #[test]
    fn verifier_rejects_bad_handler_bounds_and_catch_register() {
        let bad_range = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt, Instruction::Halt],
                vec![ExceptionHandler {
                    start: Pc::new(1),
                    end: Pc::new(1), // empty range
                    handler: Pc::new(0),
                    catch_register: Register::new(0),
                }],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_range.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::InvalidHandlerBounds { .. },
                ..
            })
        ));

        let bad_catch = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt, Instruction::Halt],
                vec![ExceptionHandler {
                    start: Pc::new(0),
                    end: Pc::new(2),
                    handler: Pc::new(1),
                    catch_register: Register::new(5), // >= register_count
                }],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            bad_catch.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::HandlerCatchRegisterOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_partially_overlapping_handlers() {
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt, Instruction::Halt, Instruction::Halt],
                vec![
                    ExceptionHandler {
                        start: Pc::new(0),
                        end: Pc::new(2),
                        handler: Pc::new(0),
                        catch_register: Register::new(0),
                    },
                    ExceptionHandler {
                        start: Pc::new(1),
                        end: Pc::new(3),
                        handler: Pc::new(2),
                        catch_register: Register::new(0),
                    },
                ],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::HandlersPartiallyOverlap { left: 0, right: 1 },
                ..
            })
        ));
    }

    #[test]
    fn verifier_accepts_disjoint_adjacent_and_nested_handlers() {
        // An outer range, an identical duplicate of it (degenerate nesting), a
        // nested child, and two adjacent disjoint siblings (`[1,4)`, `[4,7)`,
        // `[7,10)` touch at boundaries): all laminar, none partially overlapping.
        let handler = |start, end, target| ExceptionHandler {
            start: Pc::new(start),
            end: Pc::new(end),
            handler: Pc::new(target),
            catch_register: Register::new(0),
        };
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt; 10],
                vec![
                    handler(0, 10, 0),
                    handler(0, 10, 0),
                    handler(1, 4, 1),
                    handler(4, 7, 4),
                    handler(7, 10, 7),
                ],
            )],
            FunctionId::new(0),
        );
        assert!(module.verify().is_ok());
    }

    #[test]
    fn verifier_rejects_crossing_handlers_with_original_indices() {
        // `[0,10)` and `[5,15)` cross: neither disjoint nor nested. The reported
        // indices are the original `Function::handlers` positions.
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt; 15],
                vec![
                    ExceptionHandler {
                        start: Pc::new(0),
                        end: Pc::new(10),
                        handler: Pc::new(0),
                        catch_register: Register::new(0),
                    },
                    ExceptionHandler {
                        start: Pc::new(5),
                        end: Pc::new(15),
                        handler: Pc::new(5),
                        catch_register: Register::new(0),
                    },
                ],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::HandlersPartiallyOverlap { left: 0, right: 1 },
                ..
            })
        ));
    }

    #[test]
    fn verifier_accepts_and_preserves_order_of_deeply_nested_handlers() {
        // Deeply nested ranges supplied innermost-first (the reverse of the
        // sweep's internal sort order): verification accepts them and leaves
        // `Function::handlers` in its original wire order.
        const DEPTH: u32 = 64;
        let mut handlers: Vec<ExceptionHandler> = (0..DEPTH)
            .map(|level| ExceptionHandler {
                start: Pc::new(level),
                end: Pc::new(DEPTH * 2 - level),
                handler: Pc::new(level),
                catch_register: Register::new(0),
            })
            .collect();
        handlers.reverse();
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                1,
                flags(),
                vec![Instruction::Halt; (DEPTH * 2) as usize],
                handlers.clone(),
            )],
            FunctionId::new(0),
        );
        let verified = module.verify().expect("laminar nesting verifies");
        assert_eq!(verified.functions()[0].handlers(), handlers.as_slice());
    }

    #[test]
    fn verifier_rejects_facts_work_above_cap() {
        // The work-limit uses a strict `>`, so facts-work equal to the cap is
        // accepted and cap+1 is rejected. The at-cap acceptance case would force
        // the full `MAX_VERIFIER_FACTS_WORDS * 8` = 64 MiB allocation inside
        // `definite_initialization` (there is no register_count / code split
        // that hits the exact cap cheaply), so the boundary is pinned from the
        // rejection side only: one instruction over `MAX_REGISTERS` adds another
        // `MAX_REGISTERS / 64` fact words and tips the total past the cap.
        let words_per_instruction = u64::from(MAX_REGISTERS) / 64;
        let over_cap = (MAX_VERIFIER_FACTS_WORDS / words_per_instruction) as usize + 1;
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                MAX_REGISTERS,
                flags(),
                vec![Instruction::Halt; over_cap],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::VerifierWorkLimitExceeded { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_rejects_hostile_near_max_work_before_allocating() {
        // A single function whose facts would need 64 M words (512 MiB, 8x the
        // cap) is rejected in O(functions) time before any bitset is allocated:
        // the module carries only `MAX_REGISTERS` cheap `Halt` instructions.
        let module = Module::new(
            vec![],
            vec![Function::new(
                None,
                0,
                MAX_REGISTERS,
                flags(),
                vec![Instruction::Halt; MAX_REGISTERS as usize],
                vec![],
            )],
            FunctionId::new(0),
        );
        assert!(matches!(
            module.verify(),
            Err(VerifyError {
                kind: VerifyErrorKind::VerifierWorkLimitExceeded { .. },
                ..
            })
        ));
    }

    #[test]
    fn verifier_accepts_loop_carried_facts_and_exposes_certificate() {
        let module = Module::new(
            vec![Constant::Int32(0)],
            vec![Function::new(
                None,
                0,
                2,
                flags(),
                vec![
                    Instruction::LoadConst {
                        dst: Register::new(0),
                        constant: ConstantId::new(0),
                    },
                    Instruction::Binary {
                        dst: Register::new(1),
                        op: BinaryOp::Add,
                        left: Register::new(0),
                        right: Register::new(0),
                    },
                    Instruction::Jump { target: Pc::new(1) },
                ],
                vec![],
            )],
            FunctionId::new(0),
        );
        let verified = module.verify().expect("loop has a sound witness");
        let certificate = verified.certificate(FunctionId::new(0)).unwrap();
        assert_eq!(certificate.instruction_count(), 3);
        assert_eq!(
            certificate.initialized_before(Pc::new(1), Register::new(0)),
            Some(true)
        );
    }

    #[test]
    fn certificate_queries_are_total_for_all_wrappers() {
        let verified = rich_module().verify().expect("valid");
        let certificate = verified.certificate(FunctionId::new(0)).unwrap();
        // Out-of-range register -> None.
        assert_eq!(
            certificate.initialized_before(Pc::new(0), Register::new(u32::MAX)),
            None
        );
        // Out-of-range PC -> None.
        assert_eq!(
            certificate.initialized_before(Pc::new(u32::MAX), Register::new(0)),
            None
        );
        // Missing certificate for an out-of-range function -> None.
        assert!(verified.certificate(FunctionId::new(9)).is_none());
    }

    #[test]
    fn compact_scalar_layouts_are_preserved() {
        assert_eq!(std::mem::size_of::<Register>(), 4);
        assert_eq!(std::mem::size_of::<Pc>(), 4);
        assert_eq!(std::mem::size_of::<NumberBits>(), 8);
        assert_eq!(std::mem::size_of::<FunctionFlags>(), 2);
    }
}
