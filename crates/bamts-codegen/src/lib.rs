//! Shared, backend-neutral Cranelift lowering for verified BamTS bytecode.
//!
//! This crate turns a [`bamts_bytecode::Module<Verified>`] into Cranelift IR
//! through one lowering function, [`lower_module`]. It produces a stable
//! [`LoweredModule`] record (one [`ir::Function`] per bytecode function plus
//! its ABI signature, resume-dispatch tokens, and required runtime helpers)
//! that later feature-gated backends consume:
//!
//! * a `host-jit` backend that finalizes each `ir::Function` into executable
//!   memory, and
//! * an `aot` backend that emits each `ir::Function` into an object file.
//!
//! This slice performs **no** executable-memory allocation and **no** object
//! linking; it only builds and verifies IR. Both later backends supply their
//! own [`isa::TargetFrontendConfig`] (via `isa.frontend_config()`), so the ISA
//! choice, calling convention, and pointer type stay outside this crate.
//!
//! # Entry ABI
//!
//! Every lowered function has the native-entry signature from the canonical
//! execution plan (N5), matching `bamts_native::ShadowFrame` and
//! `bamts_native::Completion`:
//!
//! ```text
//! extern "C" fn(frame: *mut ShadowFrame, out: *mut Completion) -> u32
//! ```
//!
//! * `frame` points at the register frame; `frame.handles` (offset 16) is the
//!   `*mut Value` register array and `frame.bytecode_pc` (offset 8) carries the
//!   resume token (see [`Suspend`](#suspend-and-the-resume-helper)).
//! * `out` receives the completion value; the returned `u32` is a
//!   `bamts_native::CompletionTag` discriminant (`Normal`/`Throw`/`Suspend`/
//!   `FatalTrap`).
//!
//! # Register addressing convention
//!
//! Register `r[i]` lives at `frame.handles + i * 8` (one `Value`/`u64` slot).
//! Every access derives the byte offset as `i64::from(register.get()) * 8`; the
//! validation pass ([`validate_slots`]) proves this offset fits the `Offset32`
//! used by loads and stores, so `u32` register ids and CLIF addresses never mix
//! widths inconsistently. Call/construct argument windows compute a pointer
//! `handles + args_start * 8` the same way.
//!
//! # Value semantics: the explicit helper ABI
//!
//! The bytecode algebra ([`bamts_bytecode::Instruction`]) is structural: the
//! verifier proves definite initialization and CFG validity but assigns no
//! value meaning. The NaN-boxed runtime `Value` requires tag dispatch that this
//! IR-only slice must not open-code, so every operation whose result depends on
//! runtime value semantics is lowered to a call into a declared [`Helper`]
//! (`u1:<index>` external names a backend resolves to a C symbol). This crate
//! declares each helper's ABI and control-flow contract; it never defines the
//! helper body.
//!
//! Every value-producing helper follows one **completion ABI**:
//! `fn(frame, <operands…>, out: *mut Completion) -> u32(tag)`. On
//! `Normal` (0) the result is in `out.value`; on `Throw` the thrown handle is in
//! `out.value` and control routes to a covering handler; `FatalTrap` always
//! propagates to the runtime. [`Helper::Truthy`] is the sole exception: coercion
//! to boolean is total and cannot throw, so it returns the truth value directly
//! as `0`/`1` and never a completion.
//!
//! ## Opcode ledger (every variant has an explicit path)
//!
//! | Opcode           | Lowering                                                   |
//! |------------------|-----------------------------------------------------------|
//! | `LoadConst`      | [`Helper::LoadConstant`] by `ConstantId` → `dst`          |
//! | `Move`           | inline copy `handles[src]` → `handles[dst]`               |
//! | `Unary`          | [`Helper::Unary`] with the operator selector             |
//! | `Binary`         | [`Helper::Binary`] with the operator selector            |
//! | `CreateObject`   | [`Helper::CreateObject`] → `dst`                          |
//! | `CreateArray`    | [`Helper::CreateArray`] → `dst`                           |
//! | `DefineFunction` | [`Helper::DefineFunction`] by `FunctionId` → `dst`        |
//! | `GetProperty`    | [`Helper::GetProperty`] (`object`, string-`key`) → `dst`  |
//! | `SetProperty`    | [`Helper::SetProperty`] (`object`, string-`key`, `value`) |
//! | `DeleteProperty` | [`Helper::DeleteProperty`] (`object`, string-`key`) → dst |
//! | `Call`           | [`Helper::Call`] (`callee`, `this`, arg window) → `dst`   |
//! | `Construct`      | [`Helper::Construct`] (`callee`, arg window) → `dst`      |
//! | `Import`         | [`Helper::Import`] by string-`specifier` → `dst`          |
//! | `Jump`           | unconditional branch                                      |
//! | `JumpIfTrue`     | [`Helper::Truthy`] then conditional branch                |
//! | `JumpIfFalse`    | [`Helper::Truthy`] then conditional branch                |
//! | `Return`         | `handles[value]` → `out.value`, return `Normal`           |
//! | `Throw`          | route to covering handler (bind `catch_register`) or      |
//! |                  | `out.value` + return `Throw`                              |
//! | `Suspend`        | yield path + resume path via [`Helper::ResumeValue`]      |
//! | `Halt`           | `undefined` → `out.value`, return `Normal`                |
//!
//! No opcode is silently dropped and none is lowered to a placeholder no-op.
//!
//! # Exceptions
//!
//! When a completion-helper call returns `Throw` and a bytecode handler covers
//! the current pc, control branches to that handler's block after storing the
//! thrown value (`out.value`) into the handler's `catch_register` slot; the
//! explicit `Throw` opcode binds its operand into `catch_register` directly.
//! `FatalTrap` bypasses handlers. When no handler covers the pc, the completion
//! is returned to the caller.
//!
//! # Suspend and the resume helper
//!
//! `Suspend { dst, src, resume }` yields `src` and, when resumed, delivers the
//! resumed value into `dst` before continuing at `resume`. The native entry ABI
//! carries no resume input (`out.value` is the *yielded* value, not an input),
//! so the resumed value is obtained through an explicit runtime contract rather
//! than invented:
//!
//! * **Yield path** — store this suspend's resume token into `frame.bytecode_pc`
//!   (`0` is a fresh call; the suspend at bytecode pc `P` uses token `P + 1`, so
//!   tokens never collide with a fresh entry or with each other), write `src`
//!   into `out.value`, and return `Suspend`.
//! * **Resume path** — the dispatch prologue for token `P + 1` calls
//!   [`Helper::ResumeValue`], which the runtime resolves to write the verified
//!   resumed value for this frame into `out.value` (it may return `Throw` for
//!   `generator.throw`, routed to a covering handler, or `FatalTrap`); the
//!   resumed value is then stored into `dst` and control continues at `resume`.
//!
//! `bamts_bytecode` currently exposes no ABI for the resume input, so
//! [`Helper::ResumeValue`] is a **new required contract** the runtime must
//! provide for any module that suspends.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use bamts_bytecode::{
    BinaryOp, ExceptionHandler, FunctionId, Instruction, Module, Pc, Register, UnaryOp, Verified,
};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, Block, ExtFuncData, ExternalName, Function, InstBuilder, MemFlagsData, Signature,
    Type, UserExternalName, UserFuncName, Value, types,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings::{self, Flags};
use cranelift_codegen::verifier::verify_function;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

// -- ABI layout (grounded in bamts_native::ShadowFrame / Completion / Value) --

/// Byte offset of `ShadowFrame.bytecode_pc` (a `u32`).
const SHADOW_FRAME_PC_OFFSET: i32 = 8;
/// Byte offset of `ShadowFrame.handles` (a `*mut Value`).
const SHADOW_FRAME_HANDLES_OFFSET: i32 = 16;
/// Byte offset of `Completion.value` within the out-parameter.
const COMPLETION_VALUE_OFFSET: i32 = 0;
/// Size, in bytes, of one register slot (`Value` is a `u64`).
const VALUE_BYTES: i64 = 8;

/// Canonical `undefined`, matching `bamts_native::Value::UNDEFINED`
/// (`boxed(TAG_UNDEFINED=3, 0)` = `0x7ff8… | 3<<48`).
const UNDEFINED_BITS: i64 = 0x7ffb_0000_0000_0000;

/// `TrapRecordId` written to `out.value` when resumed at an unknown token.
const TRAP_INVALID_RESUME: i64 = 1;

/// [`CompletionTag::Normal`] discriminant.
const TAG_NORMAL: i64 = 0;
/// [`CompletionTag::Throw`] discriminant.
const TAG_THROW: i64 = 1;
/// [`CompletionTag::Suspend`] discriminant.
const TAG_SUSPEND: i64 = 2;
/// [`CompletionTag::FatalTrap`] discriminant.
const TAG_FATAL_TRAP: i64 = 3;

/// Cranelift external-name namespace for lowered bytecode functions: a name
/// `u0:<index>` refers to the lowered function whose [`FunctionId`] is `index`.
pub const FUNCTION_NAMESPACE: u32 = 0;
/// Cranelift external-name namespace for runtime helper imports: a name
/// `u1:<index>` refers to the [`Helper`] with [`Helper::external_index`]
/// equal to `index`.
pub const HELPER_NAMESPACE: u32 = 1;

// When the native crate is present, prove the hardcoded ABI facts still match
// its authoritative definitions. This slice's default build does not depend on
// bamts-native, so these are compiled only under the JIT-entry feature.
#[cfg(feature = "host-jit")]
const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(bamts_native::ShadowFrame, bytecode_pc) == SHADOW_FRAME_PC_OFFSET as usize);
    assert!(offset_of!(bamts_native::ShadowFrame, handles) == SHADOW_FRAME_HANDLES_OFFSET as usize);
    assert!(core::mem::size_of::<bamts_native::Completion>() == VALUE_BYTES as usize);
    assert!(bamts_native::Value::UNDEFINED.to_bits() == UNDEFINED_BITS as u64);
    assert!(bamts_native::CompletionTag::Normal.as_u32() as i64 == TAG_NORMAL);
    assert!(bamts_native::CompletionTag::Throw.as_u32() as i64 == TAG_THROW);
    assert!(bamts_native::CompletionTag::Suspend.as_u32() as i64 == TAG_SUSPEND);
    assert!(bamts_native::CompletionTag::FatalTrap.as_u32() as i64 == TAG_FATAL_TRAP);
};

// -- Runtime helpers ---------------------------------------------------------

/// A runtime routine the lowered code calls but does not define. Backends
/// resolve each [`Helper::symbol`] to an address (JIT) or relocation (AOT).
///
/// Every helper except [`Helper::Truthy`] follows the completion ABI
/// `fn(frame, <operands…>, out) -> tag`: it writes its result (or, on `Throw`,
/// the error handle) into `*out` and returns a `bamts_native::CompletionTag`.
/// [`Helper::Truthy`] performs the total ToBoolean coercion and returns `0`/`1`
/// directly, never a completion.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Helper {
    /// `bamts_load_constant(frame, const_id, out)`: materialize the module
    /// constant named by `const_id` into `out.value`.
    LoadConstant,
    /// `bamts_unary(frame, op, operand, out)`: apply the unary operator `op`
    /// (see [`unary_op_selector`]) to `operand`.
    Unary,
    /// `bamts_binary(frame, op, left, right, out)`: apply the binary operator
    /// `op` (see [`binary_op_selector`]) to `left` and `right`.
    Binary,
    /// `bamts_create_object(frame, out)`: fresh empty object into `out.value`.
    CreateObject,
    /// `bamts_create_array(frame, out)`: fresh empty array into `out.value`.
    CreateArray,
    /// `bamts_define_function(frame, function_id, out)`: closure over the named
    /// function into `out.value`.
    DefineFunction,
    /// `bamts_get_property(frame, object, key, out)`: `out.value = object[key]`,
    /// `key` naming a string constant.
    GetProperty,
    /// `bamts_set_property(frame, object, key, value, out)`: `object[key] = value`.
    SetProperty,
    /// `bamts_delete_property(frame, object, key, out)`:
    /// `out.value = delete object[key]`.
    DeleteProperty,
    /// `bamts_call(frame, callee, this, args, arg_count, out)`: call `callee`
    /// with receiver `this` over the `arg_count` values at `args`.
    Call,
    /// `bamts_construct(frame, callee, args, arg_count, out)`: construct with
    /// `callee` over the `arg_count` values at `args`.
    Construct,
    /// `bamts_import(frame, specifier, out)`: import the module named by the
    /// string constant `specifier` into `out.value`.
    Import,
    /// `bamts_truthy(frame, value) -> u32`: the total ToBoolean coercion,
    /// returning `1` when `value` is truthy and `0` otherwise. Never throws and
    /// never writes `out`.
    Truthy,
    /// `bamts_resume_value(frame, out)`: write the verified resumed value for
    /// `frame` into `out.value`. Resolves the resume-input gap in the native
    /// entry ABI (see the crate docs); may return `Throw` (`generator.throw`)
    /// or `FatalTrap`.
    ResumeValue,
}

impl Helper {
    /// The C symbol the backend links against.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Helper::LoadConstant => "bamts_load_constant",
            Helper::Unary => "bamts_unary",
            Helper::Binary => "bamts_binary",
            Helper::CreateObject => "bamts_create_object",
            Helper::CreateArray => "bamts_create_array",
            Helper::DefineFunction => "bamts_define_function",
            Helper::GetProperty => "bamts_get_property",
            Helper::SetProperty => "bamts_set_property",
            Helper::DeleteProperty => "bamts_delete_property",
            Helper::Call => "bamts_call",
            Helper::Construct => "bamts_construct",
            Helper::Import => "bamts_import",
            Helper::Truthy => "bamts_truthy",
            Helper::ResumeValue => "bamts_resume_value",
        }
    }

    /// The stable helper index within [`HELPER_NAMESPACE`]; the `index` of the
    /// `u1:<index>` external name a backend must resolve to [`Helper::symbol`].
    #[must_use]
    pub const fn external_index(self) -> u32 {
        match self {
            Helper::LoadConstant => 0,
            Helper::Unary => 1,
            Helper::Binary => 2,
            Helper::CreateObject => 3,
            Helper::CreateArray => 4,
            Helper::DefineFunction => 5,
            Helper::GetProperty => 6,
            Helper::SetProperty => 7,
            Helper::DeleteProperty => 8,
            Helper::Call => 9,
            Helper::Construct => 10,
            Helper::Import => 11,
            Helper::Truthy => 12,
            Helper::ResumeValue => 13,
        }
    }

    /// The helper for a [`HELPER_NAMESPACE`] external-name index, inverting
    /// [`Helper::external_index`]. Returns `None` for an unknown index.
    #[must_use]
    pub const fn from_external_index(index: u32) -> Option<Helper> {
        match index {
            0 => Some(Helper::LoadConstant),
            1 => Some(Helper::Unary),
            2 => Some(Helper::Binary),
            3 => Some(Helper::CreateObject),
            4 => Some(Helper::CreateArray),
            5 => Some(Helper::DefineFunction),
            6 => Some(Helper::GetProperty),
            7 => Some(Helper::SetProperty),
            8 => Some(Helper::DeleteProperty),
            9 => Some(Helper::Call),
            10 => Some(Helper::Construct),
            11 => Some(Helper::Import),
            12 => Some(Helper::Truthy),
            13 => Some(Helper::ResumeValue),
            _ => None,
        }
    }

    /// The helper parameter types, in order. `frame` and `out` pointers and
    /// runtime `Value`s are `i64`; small integer selectors and indices are
    /// `i32`.
    const fn param_types(self) -> &'static [Type] {
        match self {
            // (frame, const_id, out)
            Helper::LoadConstant => &[types::I64, types::I32, types::I64],
            // (frame, op, operand, out)
            Helper::Unary => &[types::I64, types::I32, types::I64, types::I64],
            // (frame, op, left, right, out)
            Helper::Binary => &[types::I64, types::I32, types::I64, types::I64, types::I64],
            // (frame, out)
            Helper::CreateObject | Helper::CreateArray | Helper::ResumeValue => {
                &[types::I64, types::I64]
            }
            // (frame, index, out)
            Helper::DefineFunction | Helper::Import => &[types::I64, types::I32, types::I64],
            // (frame, object, key, out)
            Helper::GetProperty | Helper::DeleteProperty => {
                &[types::I64, types::I64, types::I32, types::I64]
            }
            // (frame, object, key, value, out)
            Helper::SetProperty => &[types::I64, types::I64, types::I32, types::I64, types::I64],
            // (frame, callee, this, args, arg_count, out)
            Helper::Call => &[
                types::I64,
                types::I64,
                types::I64,
                types::I64,
                types::I32,
                types::I64,
            ],
            // (frame, callee, args, arg_count, out)
            Helper::Construct => &[types::I64, types::I64, types::I64, types::I32, types::I64],
            // (frame, value)
            Helper::Truthy => &[types::I64, types::I64],
        }
    }

    /// The helper's Cranelift signature under `call_conv`. Every helper returns
    /// an `i32` (a completion tag, or the `0`/`1` truth value for
    /// [`Helper::Truthy`]).
    fn signature(self, call_conv: CallConv) -> Signature {
        let mut signature = Signature::new(call_conv);
        for &ty in self.param_types() {
            signature.params.push(AbiParam::new(ty));
        }
        signature.returns.push(AbiParam::new(types::I32));
        signature
    }
}

/// The ABI operator selector for a unary operator, passed as the `op` argument
/// to [`Helper::Unary`]. This is the stable codegen-side operator encoding.
const fn unary_op_selector(op: UnaryOp) -> i64 {
    match op {
        UnaryOp::Void => 0,
        UnaryOp::TypeOf => 1,
        UnaryOp::Plus => 2,
        UnaryOp::Negate => 3,
        UnaryOp::BitwiseNot => 4,
        UnaryOp::LogicalNot => 5,
    }
}

/// The ABI operator selector for a binary operator, passed as the `op` argument
/// to [`Helper::Binary`]. This is the stable codegen-side operator encoding.
const fn binary_op_selector(op: BinaryOp) -> i64 {
    match op {
        BinaryOp::Add => 0,
        BinaryOp::Subtract => 1,
        BinaryOp::Multiply => 2,
        BinaryOp::Divide => 3,
        BinaryOp::Remainder => 4,
        BinaryOp::Exponent => 5,
        BinaryOp::BitAnd => 6,
        BinaryOp::BitOr => 7,
        BinaryOp::BitXor => 8,
        BinaryOp::ShiftLeft => 9,
        BinaryOp::ShiftRight => 10,
        BinaryOp::UnsignedShiftRight => 11,
        BinaryOp::Equal => 12,
        BinaryOp::NotEqual => 13,
        BinaryOp::StrictEqual => 14,
        BinaryOp::StrictNotEqual => 15,
        BinaryOp::LessThan => 16,
        BinaryOp::LessThanOrEqual => 17,
        BinaryOp::GreaterThan => 18,
        BinaryOp::GreaterThanOrEqual => 19,
        BinaryOp::InstanceOf => 20,
        BinaryOp::In => 21,
    }
}

// -- Errors ------------------------------------------------------------------

/// A deterministic, typed lowering failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LowerError {
    /// The target is not 64-bit; the `ShadowFrame`/`Value` ABI is 64-bit only.
    UnsupportedPointerWidth {
        /// The target's pointer width, in bits.
        bits: u8,
    },
    /// The module has more functions than the `u0:<index>` external-name space
    /// (`u32`) can address. Verified modules never hit this; it is surfaced
    /// explicitly rather than truncated.
    TooManyFunctions {
        /// The offending function count.
        count: usize,
    },
    /// A function's register file is too large to address with the `Offset32`
    /// used by frame loads and stores (`register_count * 8` overflows `i32`).
    /// Verified modules stay well under this; it is a codegen slot-validation
    /// guard, surfaced rather than silently truncated.
    RegisterFileTooLarge {
        /// The function whose register file is unaddressable.
        function: FunctionId,
        /// The offending register count.
        register_count: u32,
    },
    /// A produced function's signature did not match the shared native-entry
    /// ABI. This is an internal codegen invariant failure.
    EntrySignatureMismatch {
        /// The function whose signature was wrong.
        function: FunctionId,
    },
    /// Cranelift's IR verifier rejected a lowered function. This is an internal
    /// codegen invariant failure, not attacker-controlled input.
    IrVerification {
        /// The function whose IR failed verification.
        function: FunctionId,
        /// The verifier's diagnostic.
        message: String,
    },
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LowerError::UnsupportedPointerWidth { bits } => write!(
                f,
                "codegen requires a 64-bit target, but the pointer width is {bits} bits"
            ),
            LowerError::TooManyFunctions { count } => write!(
                f,
                "module has {count} functions, which exceeds the addressable u32 range"
            ),
            LowerError::RegisterFileTooLarge {
                function,
                register_count,
            } => write!(
                f,
                "function {} has {register_count} registers, whose frame offsets exceed the 32-bit slot range",
                function.get()
            ),
            LowerError::EntrySignatureMismatch { function } => write!(
                f,
                "lowered function {} does not match the native-entry ABI signature",
                function.get()
            ),
            LowerError::IrVerification { function, message } => write!(
                f,
                "Cranelift IR verification failed for function {}: {message}",
                function.get()
            ),
        }
    }
}

impl Error for LowerError {}

// -- Lowered records ---------------------------------------------------------

/// One lowered function: its Cranelift IR plus the metadata a backend needs to
/// compile and link it without re-deriving anything.
#[derive(Clone)]
pub struct LoweredFunction {
    /// The bytecode function this was lowered from.
    pub id: FunctionId,
    /// The linker symbol (`bamts_fn_<index>`).
    pub symbol: String,
    /// The entry ABI signature (identical for every function).
    pub signature: Signature,
    /// The verified Cranelift IR.
    pub clif: Function,
    /// Sorted resume-dispatch tokens the entry accepts: `0` for a fresh call,
    /// plus `P + 1` for each reachable `Suspend` at bytecode pc `P`.
    pub entry_points: Vec<u32>,
    /// The runtime helpers this function imports, sorted and deduplicated.
    pub helpers: Vec<Helper>,
}

impl fmt::Debug for LoweredFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoweredFunction")
            .field("id", &self.id.get())
            .field("symbol", &self.symbol)
            .field("entry_points", &self.entry_points)
            .field("helpers", &self.helpers)
            .finish_non_exhaustive()
    }
}

/// The complete lowering of a verified module.
#[derive(Clone)]
pub struct LoweredModule {
    /// Lowered functions, in bytecode function order.
    pub functions: Vec<LoweredFunction>,
    /// The module entry function.
    pub entry: FunctionId,
    /// The calling convention every entry and helper uses.
    pub call_conv: CallConv,
}

impl fmt::Debug for LoweredModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoweredModule")
            .field("functions", &self.functions)
            .field("entry", &self.entry.get())
            .field("call_conv", &self.call_conv)
            .finish()
    }
}

/// The linker symbol for the lowered function at `index`.
#[must_use]
pub fn function_symbol(index: u32) -> String {
    format!("bamts_fn_{index}")
}

// -- Lowering entry point ----------------------------------------------------

/// Lowers every function of a verified module to Cranelift IR.
///
/// `config` is supplied by the backend from its ISA (`isa.frontend_config()`);
/// it fixes the calling convention and pointer type. The target must be 64-bit.
/// Each produced function is validated (signature and register-slot bounds) and
/// then run through Cranelift's IR verifier before return, so a successful
/// result is structurally valid IR.
///
/// # Errors
///
/// Returns [`LowerError`] for a non-64-bit target, an unaddressable function
/// count, an unaddressable register file, a signature mismatch, or an internal
/// IR-verification failure.
pub fn lower_module(
    module: &Module<Verified>,
    config: TargetFrontendConfig,
) -> Result<LoweredModule, LowerError> {
    if config.pointer_bits() != 64 {
        return Err(LowerError::UnsupportedPointerWidth {
            bits: config.pointer_bits(),
        });
    }
    let function_count = module.functions().len();
    if u32::try_from(function_count).is_err() {
        return Err(LowerError::TooManyFunctions {
            count: function_count,
        });
    }

    let call_conv = config.default_call_conv;
    let entry_signature = entry_signature(call_conv);
    let flags = Flags::new(settings::builder());
    let mut builder_context = FunctionBuilderContext::new();

    let mut functions = Vec::with_capacity(function_count);
    for (index, function) in module.functions().iter().enumerate() {
        // Bounds checked above.
        let id = FunctionId::new(index as u32);
        let lowered = lower_function(
            id,
            function,
            &entry_signature,
            config,
            &flags,
            &mut builder_context,
        )?;
        functions.push(lowered);
    }

    Ok(LoweredModule {
        functions,
        entry: module.entry(),
        call_conv,
    })
}

/// The shared native-entry signature: `(frame, out) -> tag`.
fn entry_signature(call_conv: CallConv) -> Signature {
    let mut signature = Signature::new(call_conv);
    signature.params.push(AbiParam::new(types::I64)); // *mut ShadowFrame
    signature.params.push(AbiParam::new(types::I64)); // *mut Completion
    signature.returns.push(AbiParam::new(types::I32)); // CompletionTag
    signature
}

/// Validates that a function's register file is addressable with the 32-bit
/// slot offsets used by every frame load and store. The bytecode verifier
/// proves register references stay within `register_count`; this proves
/// `register_count` itself fits the CLIF addressing convention.
fn validate_slots(id: FunctionId, function: &bamts_bytecode::Function) -> Result<(), LowerError> {
    let register_count = function.register_count();
    let addressable = i64::from(register_count)
        .checked_mul(VALUE_BYTES)
        .is_some_and(|bytes| i32::try_from(bytes).is_ok());
    if addressable {
        Ok(())
    } else {
        Err(LowerError::RegisterFileTooLarge {
            function: id,
            register_count,
        })
    }
}

fn lower_function(
    id: FunctionId,
    function: &bamts_bytecode::Function,
    entry_signature: &Signature,
    config: TargetFrontendConfig,
    flags: &Flags,
    builder_context: &mut FunctionBuilderContext,
) -> Result<LoweredFunction, LowerError> {
    validate_slots(id, function)?;

    let code = function.code();
    let handlers = function.handlers();
    let reachable = reachable_pcs(code, handlers);
    let entry_points = resume_tokens(code, &reachable);

    let name = UserFuncName::user(FUNCTION_NAMESPACE, id.get());
    let mut clif = Function::with_name_signature(name, entry_signature.clone());

    let helpers = {
        let builder = FunctionBuilder::new(&mut clif, builder_context);
        let mut lowering = Lowering::new(builder, code.len(), handlers, config.default_call_conv);
        lowering.build(code, &reachable);
        lowering.finish(config)
    };

    // Signature validation: the produced entry ABI must be identical to the
    // shared native-entry signature (params and returns), independent of the
    // structural IR checks Cranelift performs below.
    if clif.signature != *entry_signature {
        return Err(LowerError::EntrySignatureMismatch { function: id });
    }

    verify_function(&clif, flags).map_err(|errors| LowerError::IrVerification {
        function: id,
        message: errors.to_string(),
    })?;

    Ok(LoweredFunction {
        id,
        symbol: function_symbol(id.get()),
        signature: entry_signature.clone(),
        clif,
        entry_points,
        helpers,
    })
}

// -- Per-function lowering ---------------------------------------------------

struct Lowering<'a> {
    builder: FunctionBuilder<'a>,
    /// One block per reachable bytecode pc; `None` for unreachable pcs, which
    /// are never emitted (keeping every block dominated by the entry block).
    pc_blocks: Vec<Option<Block>>,
    /// The resume prologue block for each reachable `Suspend`, keyed by the
    /// suspend's bytecode pc.
    resume_blocks: BTreeMap<usize, Block>,
    handlers: &'a [ExceptionHandler],
    call_conv: CallConv,
    frame: Value,
    out: Value,
    helper_refs: BTreeMap<Helper, cranelift_codegen::ir::FuncRef>,
}

impl<'a> Lowering<'a> {
    fn new(
        mut builder: FunctionBuilder<'a>,
        code_len: usize,
        handlers: &'a [ExceptionHandler],
        call_conv: CallConv,
    ) -> Self {
        let pc_blocks = vec![None; code_len];
        // The entry/dispatch block owns the function parameters. Reachable pc
        // blocks are created up front in ascending order for deterministic IR.
        let dispatch = builder.create_block();
        builder.append_block_params_for_function_params(dispatch);
        builder.switch_to_block(dispatch);
        let frame = builder.block_params(dispatch)[0];
        let out = builder.block_params(dispatch)[1];
        Self {
            builder,
            pc_blocks,
            resume_blocks: BTreeMap::new(),
            handlers,
            call_conv,
            frame,
            out,
            helper_refs: BTreeMap::new(),
        }
    }

    fn build(&mut self, code: &[Instruction], reachable: &BTreeSet<usize>) {
        for &pc in reachable {
            self.pc_blocks[pc] = Some(self.builder.create_block());
        }
        for &pc in reachable {
            if let Instruction::Suspend { .. } = code[pc] {
                let block = self.builder.create_block();
                self.resume_blocks.insert(pc, block);
            }
        }
        self.emit_dispatch(reachable);
        for &pc in reachable {
            self.emit_instruction(pc, code[pc]);
        }
        for &pc in reachable {
            if let Instruction::Suspend { dst, resume, .. } = code[pc] {
                self.emit_resume_prologue(pc, dst, resume);
            }
        }
        self.builder.seal_all_blocks();
    }

    /// Consumes the builder, returning the imported helpers in a stable order.
    fn finish(self, config: TargetFrontendConfig) -> Vec<Helper> {
        let helpers = self.helper_refs.keys().copied().collect();
        self.builder.finalize(config);
        helpers
    }

    /// Emits the entry block: select the starting block from the resume token in
    /// `frame.bytecode_pc`. Token `0` is a fresh call (pc-0 block); token `P + 1`
    /// enters the resume prologue for the suspend at pc `P`.
    fn emit_dispatch(&mut self, reachable: &BTreeSet<usize>) {
        // A function with no suspends has a single entry (token 0): fresh calls
        // always begin at pc 0, so no token comparison is emitted.
        if self.resume_blocks.is_empty() {
            let target = self.pc_blocks[0].expect("entry pc is reachable");
            self.builder.ins().jump(target, &[]);
            return;
        }
        let _ = reachable;

        let token = self.builder.ins().load(
            types::I32,
            MemFlagsData::trusted(),
            self.frame,
            SHADOW_FRAME_PC_OFFSET,
        );
        // Fresh entry (token 0).
        let fresh = self.pc_blocks[0].expect("entry pc is reachable");
        let after_fresh = self.builder.create_block();
        let is_fresh = self.builder.ins().icmp_imm_u(IntCC::Equal, token, 0);
        self.builder
            .ins()
            .brif(is_fresh, fresh, &[], after_fresh, &[]);
        self.builder.switch_to_block(after_fresh);
        // Resume tokens (P + 1), in ascending pc order for deterministic IR.
        let resume: Vec<(usize, Block)> = self
            .resume_blocks
            .iter()
            .map(|(&pc, &block)| (pc, block))
            .collect();
        for (pc, block) in resume {
            let token_value = i64::from(pc as u32 + 1);
            let matches = self
                .builder
                .ins()
                .icmp_imm_u(IntCC::Equal, token, token_value);
            let next = self.builder.create_block();
            self.builder.ins().brif(matches, block, &[], next, &[]);
            self.builder.switch_to_block(next);
        }
        // Resumed at an unrecognized token: fatal trap back to the runtime.
        self.emit_trap(TRAP_INVALID_RESUME);
    }

    fn emit_instruction(&mut self, pc: usize, instruction: Instruction) {
        let block = self.pc_blocks[pc].expect("reachable pc has a block");
        self.builder.switch_to_block(block);
        match instruction {
            Instruction::LoadConst { dst, constant } => {
                let const_id = self.iconst32(i64::from(constant.get()));
                let tag = self.call_helper(Helper::LoadConstant, &[self.frame, const_id, self.out]);
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Move { dst, src } => {
                let handles = self.load_handles();
                let value = self.load_register(handles, src);
                self.store_register(handles, dst, value);
                self.jump_to_next(pc);
            }
            Instruction::Unary { dst, op, operand } => {
                let handles = self.load_handles();
                let operand_value = self.load_register(handles, operand);
                let selector = self.iconst32(unary_op_selector(op));
                let tag = self.call_helper(
                    Helper::Unary,
                    &[self.frame, selector, operand_value, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Binary {
                dst,
                op,
                left,
                right,
            } => {
                let handles = self.load_handles();
                let left_value = self.load_register(handles, left);
                let right_value = self.load_register(handles, right);
                let selector = self.iconst32(binary_op_selector(op));
                let tag = self.call_helper(
                    Helper::Binary,
                    &[self.frame, selector, left_value, right_value, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::CreateObject { dst } => {
                let tag = self.call_helper(Helper::CreateObject, &[self.frame, self.out]);
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::CreateArray { dst } => {
                let tag = self.call_helper(Helper::CreateArray, &[self.frame, self.out]);
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::DefineFunction { dst, function } => {
                let function_id = self.iconst32(i64::from(function.get()));
                let tag =
                    self.call_helper(Helper::DefineFunction, &[self.frame, function_id, self.out]);
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::GetProperty { dst, object, key } => {
                let handles = self.load_handles();
                let object_value = self.load_register(handles, object);
                let key_id = self.iconst32(i64::from(key.get()));
                let tag = self.call_helper(
                    Helper::GetProperty,
                    &[self.frame, object_value, key_id, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::SetProperty { object, key, value } => {
                let handles = self.load_handles();
                let object_value = self.load_register(handles, object);
                let value_value = self.load_register(handles, value);
                let key_id = self.iconst32(i64::from(key.get()));
                let tag = self.call_helper(
                    Helper::SetProperty,
                    &[self.frame, object_value, key_id, value_value, self.out],
                );
                self.route_completion(pc, tag, None);
            }
            Instruction::DeleteProperty { dst, object, key } => {
                let handles = self.load_handles();
                let object_value = self.load_register(handles, object);
                let key_id = self.iconst32(i64::from(key.get()));
                let tag = self.call_helper(
                    Helper::DeleteProperty,
                    &[self.frame, object_value, key_id, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Call {
                dst,
                callee,
                this_value,
                args_start,
                arg_count,
            } => {
                let handles = self.load_handles();
                let callee_value = self.load_register(handles, callee);
                let this = self.load_register(handles, this_value);
                let args = self.argument_window(handles, args_start);
                let count = self.iconst32(i64::from(arg_count));
                let tag = self.call_helper(
                    Helper::Call,
                    &[self.frame, callee_value, this, args, count, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Construct {
                dst,
                callee,
                args_start,
                arg_count,
            } => {
                let handles = self.load_handles();
                let callee_value = self.load_register(handles, callee);
                let args = self.argument_window(handles, args_start);
                let count = self.iconst32(i64::from(arg_count));
                let tag = self.call_helper(
                    Helper::Construct,
                    &[self.frame, callee_value, args, count, self.out],
                );
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Import { dst, specifier } => {
                let specifier_id = self.iconst32(i64::from(specifier.get()));
                let tag =
                    self.call_helper(Helper::Import, &[self.frame, specifier_id, self.out]);
                self.route_completion(pc, tag, Some(dst));
            }
            Instruction::Jump { target } => {
                let target = self.pc_block(target);
                self.builder.ins().jump(target, &[]);
            }
            Instruction::JumpIfTrue { condition, target } => {
                self.emit_conditional(pc, condition, target.get() as usize, pc + 1);
            }
            Instruction::JumpIfFalse { condition, target } => {
                self.emit_conditional(pc, condition, pc + 1, target.get() as usize);
            }
            Instruction::Return { value } => self.emit_return(value),
            Instruction::Throw { value } => self.emit_throw(pc, value),
            Instruction::Suspend { src, .. } => self.emit_suspend(pc, src),
            Instruction::Halt => self.emit_halt(),
        }
    }

    /// Routes a value-helper completion: on `Normal`, store `out.value` into
    /// `dst` (when the opcode defines one) and continue; otherwise route the
    /// abnormal completion to a handler or the caller.
    fn route_completion(&mut self, pc: usize, tag: Value, dst: Option<Register>) {
        let normal = self.builder.create_block();
        let abnormal = self.builder.create_block();
        // Normal (tag == 0) takes `normal`; any nonzero tag is abnormal.
        self.builder.ins().brif(tag, abnormal, &[], normal, &[]);

        self.builder.switch_to_block(normal);
        if let Some(dst) = dst {
            let handles = self.load_handles();
            let result = self.load_completion_value();
            self.store_register(handles, dst, result);
        }
        self.jump_to_next(pc);

        self.builder.switch_to_block(abnormal);
        self.emit_abnormal_completion(pc, tag);
    }

    /// Routes a nonzero completion tag: to a covering handler on `Throw` (the
    /// thrown value is bound into the handler's `catch_register`), otherwise
    /// propagated to the caller. `FatalTrap` never enters a handler.
    fn emit_abnormal_completion(&mut self, pc: usize, tag: Value) {
        match innermost_handler(self.handlers, pc) {
            Some(handler) => {
                let handler_block = self.pc_block(handler.handler);
                let bind = self.builder.create_block();
                let propagate = self.builder.create_block();
                let is_throw = self.builder.ins().icmp_imm_u(IntCC::Equal, tag, TAG_THROW);
                self.builder.ins().brif(is_throw, bind, &[], propagate, &[]);

                self.builder.switch_to_block(bind);
                let handles = self.load_handles();
                let thrown = self.load_completion_value();
                self.store_register(handles, handler.catch_register, thrown);
                self.builder.ins().jump(handler_block, &[]);

                self.builder.switch_to_block(propagate);
                self.builder.ins().return_(&[tag]);
            }
            None => {
                self.builder.ins().return_(&[tag]);
            }
        }
    }

    /// Emits `JumpIfTrue`/`JumpIfFalse`: coerce `condition` to boolean via the
    /// total [`Helper::Truthy`], then branch to `true_target` on truthy and
    /// `false_target` otherwise.
    fn emit_conditional(
        &mut self,
        pc: usize,
        condition: Register,
        true_target: usize,
        false_target: usize,
    ) {
        let _ = pc;
        let handles = self.load_handles();
        let condition_value = self.load_register(handles, condition);
        let truth = self.call_helper(Helper::Truthy, &[self.frame, condition_value]);
        let then_block = self.pc_blocks[true_target].expect("branch target is reachable");
        let else_block = self.pc_blocks[false_target].expect("branch target is reachable");
        self.builder
            .ins()
            .brif(truth, then_block, &[], else_block, &[]);
    }

    fn emit_return(&mut self, value: Register) {
        let handles = self.load_handles();
        let return_value = self.load_register(handles, value);
        self.store_completion_value(return_value);
        let tag = self.iconst32(TAG_NORMAL);
        self.builder.ins().return_(&[tag]);
    }

    /// Emits `Throw`: bind the thrown value into a covering handler's
    /// `catch_register` and branch there, or write it to `out.value` and return
    /// `Throw` to the caller.
    fn emit_throw(&mut self, pc: usize, value: Register) {
        let handles = self.load_handles();
        let thrown = self.load_register(handles, value);
        match innermost_handler(self.handlers, pc) {
            Some(handler) => {
                self.store_register(handles, handler.catch_register, thrown);
                let handler_block = self.pc_block(handler.handler);
                self.builder.ins().jump(handler_block, &[]);
            }
            None => {
                self.store_completion_value(thrown);
                let tag = self.iconst32(TAG_THROW);
                self.builder.ins().return_(&[tag]);
            }
        }
    }

    /// Emits the `Suspend` yield path: store this suspend's resume token into
    /// `frame.bytecode_pc`, yield `src` in `out.value`, and return `Suspend`.
    fn emit_suspend(&mut self, pc: usize, src: Register) {
        let token = self.iconst32(i64::from(pc as u32 + 1));
        self.builder.ins().store(
            MemFlagsData::trusted(),
            token,
            self.frame,
            SHADOW_FRAME_PC_OFFSET,
        );
        let handles = self.load_handles();
        let yielded = self.load_register(handles, src);
        self.store_completion_value(yielded);
        let tag = self.iconst32(TAG_SUSPEND);
        self.builder.ins().return_(&[tag]);
    }

    /// Emits a `Suspend` resume prologue: obtain the resumed value from the
    /// runtime via [`Helper::ResumeValue`], store it into `dst`, and continue at
    /// `resume`. A `Throw` from the resume (e.g. `generator.throw`) routes to a
    /// covering handler; `FatalTrap` propagates.
    fn emit_resume_prologue(&mut self, pc: usize, dst: Register, resume: Pc) {
        let block = self.resume_blocks[&pc];
        self.builder.switch_to_block(block);
        let tag = self.call_helper(Helper::ResumeValue, &[self.frame, self.out]);

        let normal = self.builder.create_block();
        let abnormal = self.builder.create_block();
        self.builder.ins().brif(tag, abnormal, &[], normal, &[]);

        self.builder.switch_to_block(normal);
        let handles = self.load_handles();
        let resumed = self.load_completion_value();
        self.store_register(handles, dst, resumed);
        let target = self.pc_block(resume);
        self.builder.ins().jump(target, &[]);

        self.builder.switch_to_block(abnormal);
        self.emit_abnormal_completion(pc, tag);
    }

    fn emit_halt(&mut self) {
        let undefined = self.builder.ins().iconst(types::I64, UNDEFINED_BITS);
        self.store_completion_value(undefined);
        let tag = self.iconst32(TAG_NORMAL);
        self.builder.ins().return_(&[tag]);
    }

    fn emit_trap(&mut self, trap_id: i64) {
        let value = self.builder.ins().iconst(types::I64, trap_id);
        self.store_completion_value(value);
        let tag = self.iconst32(TAG_FATAL_TRAP);
        self.builder.ins().return_(&[tag]);
    }

    fn jump_to_next(&mut self, pc: usize) {
        let next = self.pc_blocks[pc + 1].expect("fallthrough successor is reachable");
        self.builder.ins().jump(next, &[]);
    }

    /// The Cranelift block for a bytecode target pc.
    fn pc_block(&self, target: Pc) -> Block {
        self.pc_blocks[target.get() as usize].expect("control-flow target is reachable")
    }

    fn iconst32(&mut self, value: i64) -> Value {
        self.builder.ins().iconst(types::I32, value)
    }

    fn load_handles(&mut self) -> Value {
        // Re-read on each use: a helper call may relocate the register array.
        self.builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.frame,
            SHADOW_FRAME_HANDLES_OFFSET,
        )
    }

    fn load_register(&mut self, handles: Value, register: Register) -> Value {
        self.builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            handles,
            register_offset(register),
        )
    }

    fn store_register(&mut self, handles: Value, register: Register, value: Value) {
        self.builder.ins().store(
            MemFlagsData::trusted(),
            value,
            handles,
            register_offset(register),
        );
    }

    fn load_completion_value(&mut self) -> Value {
        self.builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.out,
            COMPLETION_VALUE_OFFSET,
        )
    }

    fn store_completion_value(&mut self, value: Value) {
        self.builder.ins().store(
            MemFlagsData::trusted(),
            value,
            self.out,
            COMPLETION_VALUE_OFFSET,
        );
    }

    /// The base pointer of the argument window `[args_start, …)`: the register
    /// array plus `args_start * 8`. Uses `i64` pointer arithmetic so windows in
    /// large register files stay addressable.
    fn argument_window(&mut self, handles: Value, args_start: Register) -> Value {
        let offset = i64::from(args_start.get()) * VALUE_BYTES;
        if offset == 0 {
            handles
        } else {
            let delta = self.builder.ins().iconst(types::I64, offset);
            self.builder.ins().iadd(handles, delta)
        }
    }

    fn call_helper(&mut self, helper: Helper, args: &[Value]) -> Value {
        let func_ref = self.helper_ref(helper);
        let call = self.builder.ins().call(func_ref, args);
        self.builder.inst_results(call)[0]
    }

    fn helper_ref(&mut self, helper: Helper) -> cranelift_codegen::ir::FuncRef {
        if let Some(&func_ref) = self.helper_refs.get(&helper) {
            return func_ref;
        }
        let signature = helper.signature(self.call_conv);
        let sig_ref = self.builder.import_signature(signature);
        let name = self
            .builder
            .func
            .declare_imported_user_function(UserExternalName::new(
                HELPER_NAMESPACE,
                helper.external_index(),
            ));
        let func_ref = self.builder.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature: sig_ref,
            colocated: false,
            patchable: false,
        });
        self.helper_refs.insert(helper, func_ref);
        func_ref
    }
}

/// The byte offset of a register slot within the handles array. [`validate_slots`]
/// proves `register_count * 8` fits `i32`, so this conversion never truncates.
fn register_offset(register: Register) -> i32 {
    i32::try_from(i64::from(register.get()) * VALUE_BYTES).expect("register slot offset fits i32")
}

// -- CFG analysis ------------------------------------------------------------

/// The set of pcs reachable from a fresh call (pc 0) plus every resumable
/// suspend point, following fallthrough, jumps, conditional targets, suspend
/// resumes, and the handler edge of any instruction that can route a `Throw`
/// completion into a covering handler.
fn reachable_pcs(code: &[Instruction], handlers: &[ExceptionHandler]) -> BTreeSet<usize> {
    let mut reachable = BTreeSet::new();
    let mut worklist = Vec::new();
    if !code.is_empty() {
        worklist.push(0usize);
    }
    while let Some(pc) = worklist.pop() {
        if !reachable.insert(pc) {
            continue;
        }
        let instruction = code[pc];
        instruction.visit_normal_successors(pc, |target| worklist.push(target));
        if routes_to_handler(instruction) {
            if let Some(handler) = innermost_handler(handlers, pc) {
                worklist.push(handler.handler.get() as usize);
            }
        }
    }
    reachable
}

/// Whether an instruction can transfer control to a covering handler: the
/// explicit `Throw`, or any completion-helper opcode whose abnormal path may
/// return `Throw`. Keeping this exhaustive guarantees every handler block a
/// throwing opcode targets is marked reachable and thus emitted.
fn routes_to_handler(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadConst { .. }
            | Instruction::Unary { .. }
            | Instruction::Binary { .. }
            | Instruction::CreateObject { .. }
            | Instruction::CreateArray { .. }
            | Instruction::DefineFunction { .. }
            | Instruction::GetProperty { .. }
            | Instruction::SetProperty { .. }
            | Instruction::DeleteProperty { .. }
            | Instruction::Call { .. }
            | Instruction::Construct { .. }
            | Instruction::Import { .. }
            | Instruction::Suspend { .. }
            | Instruction::Throw { .. }
    )
}

/// The resume-dispatch tokens the entry accepts: `0` (fresh call) plus `P + 1`
/// for each reachable `Suspend` at bytecode pc `P`, sorted and deduplicated.
fn resume_tokens(code: &[Instruction], reachable: &BTreeSet<usize>) -> Vec<u32> {
    let mut tokens = BTreeSet::new();
    if !code.is_empty() {
        tokens.insert(0u32);
    }
    for &pc in reachable {
        if let Instruction::Suspend { .. } = code[pc] {
            tokens.insert(pc as u32 + 1);
        }
    }
    tokens.into_iter().collect()
}

/// The innermost handler whose half-open `[start, end)` interval covers `pc`.
///
/// The verifier proves handlers are pairwise nested or disjoint, so "innermost"
/// is the covering handler with the greatest start (ties broken by smallest end,
/// then latest index) — a total, deterministic order.
fn innermost_handler(handlers: &[ExceptionHandler], pc: usize) -> Option<ExceptionHandler> {
    let pc = pc as u32;
    let mut best: Option<(usize, ExceptionHandler)> = None;
    for (index, handler) in handlers.iter().copied().enumerate() {
        if handler.start.get() > pc || pc >= handler.end.get() {
            continue;
        }
        let is_better = match best {
            None => true,
            Some((best_index, current)) => {
                (handler.start.get(), current.end.get(), index)
                    > (current.start.get(), handler.end.get(), best_index)
            }
        };
        if is_better {
            best = Some((index, handler));
        }
    }
    best.map(|(_, handler)| handler)
}

trait NormalSuccessors {
    /// Visits each normal-control successor pc (excluding handler edges).
    fn visit_normal_successors(self, pc: usize, visit: impl FnMut(usize));
}

impl NormalSuccessors for Instruction {
    fn visit_normal_successors(self, pc: usize, mut visit: impl FnMut(usize)) {
        match self {
            Instruction::Jump { target } => visit(target.get() as usize),
            Instruction::JumpIfTrue { target, .. } | Instruction::JumpIfFalse { target, .. } => {
                visit(target.get() as usize);
                visit(pc + 1);
            }
            // A suspend returns now; its `resume` pc is entered by a later call
            // through the resume prologue.
            Instruction::Suspend { resume, .. } => visit(resume.get() as usize),
            Instruction::Return { .. } | Instruction::Throw { .. } | Instruction::Halt => {}
            Instruction::LoadConst { .. }
            | Instruction::Move { .. }
            | Instruction::Unary { .. }
            | Instruction::Binary { .. }
            | Instruction::CreateObject { .. }
            | Instruction::CreateArray { .. }
            | Instruction::DefineFunction { .. }
            | Instruction::GetProperty { .. }
            | Instruction::SetProperty { .. }
            | Instruction::DeleteProperty { .. }
            | Instruction::Call { .. }
            | Instruction::Construct { .. }
            | Instruction::Import { .. } => visit(pc + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_bytecode::{
        Constant, ConstantId, Function as BytecodeFunction, FunctionFlags, Instruction, Pc,
        Register,
    };
    use cranelift_codegen::isa;

    /// A frontend config for the host target, without naming `target_lexicon`.
    fn host_config() -> TargetFrontendConfig {
        let flags = Flags::new(settings::builder());
        for name in [
            "x86_64",
            "aarch64",
            "riscv64",
            "s390x",
            "x86_64-unknown-linux-gnu",
        ] {
            if let Ok(builder) = isa::lookup_by_name(name) {
                if let Ok(target) = builder.finish(flags.clone()) {
                    return target.frontend_config();
                }
            }
        }
        panic!("no native ISA available for tests");
    }

    fn reg(index: u32) -> Register {
        Register::new(index)
    }

    fn func(
        register_count: u32,
        code: Vec<Instruction>,
        handlers: Vec<ExceptionHandler>,
    ) -> BytecodeFunction {
        BytecodeFunction::new(
            None,
            0,
            register_count,
            FunctionFlags::default(),
            code,
            handlers,
        )
    }

    fn verified(constants: Vec<Constant>, functions: Vec<BytecodeFunction>) -> Module<Verified> {
        Module::new(constants, functions, FunctionId::new(0))
            .verify()
            .expect("test module verifies")
    }

    fn single(function: BytecodeFunction) -> Module<Verified> {
        verified(vec![Constant::Undefined], vec![function])
    }

    /// Load `Undefined` (constant 0) into `dst`; a cheap way to satisfy the
    /// definite-initialization verifier before a register is read.
    fn load_undef(dst: Register) -> Instruction {
        Instruction::LoadConst {
            dst,
            constant: ConstantId::new(0),
        }
    }

    fn clif_of(module: &Module<Verified>) -> String {
        let lowered = lower_module(module, host_config()).expect("lowers");
        lowered.functions[0].clif.display().to_string()
    }

    #[test]
    fn entry_signature_is_the_native_abi() {
        let module = single(func(1, vec![Instruction::Halt], Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        let signature = &function.signature;
        assert_eq!(signature.params.len(), 2);
        assert_eq!(signature.params[0].value_type, types::I64);
        assert_eq!(signature.params[1].value_type, types::I64);
        assert_eq!(signature.returns.len(), 1);
        assert_eq!(signature.returns[0].value_type, types::I32);
        assert_eq!(function.symbol, "bamts_fn_0");
        assert_eq!(function.id.get(), 0);
        assert_eq!(lowered.entry.get(), 0);
    }

    #[test]
    fn halt_only_function_returns_normal_with_undefined() {
        let module = single(func(1, vec![Instruction::Halt], Vec::new()));
        let clif = clif_of(&module);
        // Single entry (token 0): no resume-token load.
        assert!(
            !clif.contains("load.i32"),
            "no dispatch load expected:\n{clif}"
        );
        assert!(
            clif.contains("0x7ffb_0000_0000_0000"),
            "undefined store missing:\n{clif}"
        );
        assert!(clif.contains("return"), "must return:\n{clif}");
        let lowered = lower_module(&module, host_config()).expect("lowers");
        assert!(lowered.functions[0].helpers.is_empty());
        assert_eq!(lowered.functions[0].entry_points, vec![0]);
    }

    #[test]
    fn load_const_routes_through_the_constant_helper() {
        let module = single(func(
            1,
            vec![load_undef(reg(0)), Instruction::Halt],
            Vec::new(),
        ));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert_eq!(function.helpers, vec![Helper::LoadConstant]);
        assert_eq!(Helper::LoadConstant.symbol(), "bamts_load_constant");
        assert_eq!(Helper::LoadConstant.external_index(), 0);
        assert_eq!(Helper::from_external_index(0), Some(Helper::LoadConstant));
        let clif = function.clif.display().to_string();
        assert!(
            clif.contains("u1:0"),
            "constant helper import missing:\n{clif}"
        );
        assert!(
            clif.contains("(i64, i32, i64) -> i32"),
            "constant helper sig wrong:\n{clif}"
        );
        assert!(clif.contains("call"), "helper call missing:\n{clif}");
    }

    #[test]
    fn binary_routes_through_the_binary_helper() {
        let code = vec![
            load_undef(reg(0)),
            load_undef(reg(1)),
            Instruction::Binary {
                dst: reg(2),
                op: BinaryOp::Add,
                left: reg(0),
                right: reg(1),
            },
            Instruction::Halt,
        ];
        let module = single(func(3, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::Binary));
        assert_eq!(Helper::Binary.external_index(), 2);
        let clif = function.clif.display().to_string();
        assert!(
            clif.contains("u1:2"),
            "binary helper import missing:\n{clif}"
        );
        assert!(
            clif.contains("(i64, i32, i64, i64) -> i32"),
            "binary helper sig wrong:\n{clif}"
        );
    }

    #[test]
    fn unary_routes_through_the_unary_helper() {
        let code = vec![
            load_undef(reg(0)),
            Instruction::Unary {
                dst: reg(1),
                op: UnaryOp::Negate,
                operand: reg(0),
            },
            Instruction::Halt,
        ];
        let module = single(func(2, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::Unary));
        let clif = function.clif.display().to_string();
        assert!(
            clif.contains("u1:1"),
            "unary helper import missing:\n{clif}"
        );
        assert!(
            clif.contains("(i64, i32, i64) -> i32"),
            "unary helper sig wrong:\n{clif}"
        );
    }

    #[test]
    fn move_copies_registers_without_a_helper() {
        let code = vec![
            load_undef(reg(0)),
            Instruction::Move {
                dst: reg(1),
                src: reg(0),
            },
            Instruction::Halt,
        ];
        let module = single(func(2, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        // Move introduces no helper of its own.
        assert_eq!(lowered.functions[0].helpers, vec![Helper::LoadConstant]);
    }

    #[test]
    fn conditional_branch_coerces_via_truthy() {
        // r0 = undef; if truthy(r0) goto 3 else fall to 2; both halt.
        let code = vec![
            load_undef(reg(0)),
            Instruction::JumpIfTrue {
                condition: reg(0),
                target: Pc::new(3),
            },
            Instruction::Halt,
            Instruction::Halt,
        ];
        let module = single(func(1, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::Truthy));
        assert_eq!(Helper::Truthy.external_index(), 12);
        let clif = function.clif.display().to_string();
        assert!(
            clif.contains("u1:12"),
            "truthy helper import missing:\n{clif}"
        );
        // Truthy is total: two i64 params, one i32 result, no out-parameter.
        assert!(
            clif.contains("(i64, i64) -> i32"),
            "truthy helper sig wrong:\n{clif}"
        );
        assert!(clif.contains("brif"), "conditional branch missing:\n{clif}");
    }

    #[test]
    fn suspend_uses_a_resume_token_and_resume_helper() {
        // r0 = undef; suspend (yield r0), resume at pc 2; pc 2 halts.
        let code = vec![
            load_undef(reg(0)),
            Instruction::Suspend {
                dst: reg(0),
                src: reg(0),
                resume: Pc::new(2),
            },
            Instruction::Halt,
        ];
        let module = single(func(1, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        // Fresh token 0 plus the suspend at pc 1 -> token 2.
        assert_eq!(function.entry_points, vec![0, 2]);
        assert!(function.helpers.contains(&Helper::ResumeValue));
        assert_eq!(Helper::ResumeValue.external_index(), 13);
        let clif = function.clif.display().to_string();
        // Multi-token dispatch loads and compares the resume token.
        assert!(
            clif.contains("load.i32"),
            "dispatch token load missing:\n{clif}"
        );
        assert!(clif.contains("icmp"), "dispatch compare missing:\n{clif}");
        // The suspend stores its resume token into the frame.
        assert!(
            clif.contains("store"),
            "resume token store missing:\n{clif}"
        );
        assert!(
            clif.contains("u1:13"),
            "resume helper import missing:\n{clif}"
        );
    }

    #[test]
    fn throwing_op_under_a_handler_routes_and_binds_catch_register() {
        // handler covers [0,3), dispatches to pc 3 binding into r0.
        let code = vec![
            load_undef(reg(0)),
            load_undef(reg(1)),
            Instruction::Binary {
                dst: reg(2),
                op: BinaryOp::Add,
                left: reg(0),
                right: reg(1),
            },
            Instruction::Halt,
        ];
        let handlers = vec![ExceptionHandler {
            start: Pc::new(0),
            end: Pc::new(3),
            handler: Pc::new(3),
            catch_register: reg(0),
        }];
        let module = single(func(3, code, handlers));
        let clif = clif_of(&module);
        // Around the throwing Binary: normal-vs-abnormal, then throw-vs-propagate.
        let brif_count = clif.matches("brif").count();
        assert!(
            brif_count >= 2,
            "expected handler routing brifs, got {brif_count}:\n{clif}"
        );
        assert!(
            clif.contains("icmp"),
            "throw discriminator missing:\n{clif}"
        );
    }

    #[test]
    fn explicit_throw_binds_catch_register_and_jumps() {
        // r0 = undef; throw r0; handler at pc 2 binds into r0.
        let code = vec![
            load_undef(reg(0)),
            Instruction::Throw { value: reg(0) },
            Instruction::Halt,
        ];
        let handlers = vec![ExceptionHandler {
            start: Pc::new(0),
            end: Pc::new(2),
            handler: Pc::new(2),
            catch_register: reg(0),
        }];
        let module = single(func(1, code, handlers));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        // A locally-caught throw needs no helper and jumps to the handler.
        assert_eq!(lowered.functions[0].helpers, vec![Helper::LoadConstant]);
        let clif = lowered.functions[0].clif.display().to_string();
        assert!(clif.contains("jump"), "handler jump missing:\n{clif}");
        assert!(
            clif.contains("store"),
            "catch-register bind missing:\n{clif}"
        );
    }

    #[test]
    fn return_writes_completion_and_normal_tag() {
        let code = vec![load_undef(reg(0)), Instruction::Return { value: reg(0) }];
        let module = single(func(1, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        assert_eq!(lowered.functions[0].helpers, vec![Helper::LoadConstant]);
        let clif = lowered.functions[0].clif.display().to_string();
        assert!(
            clif.contains("store"),
            "return value store missing:\n{clif}"
        );
        assert!(clif.contains("return"), "return missing:\n{clif}");
    }

    #[test]
    fn call_computes_argument_window_and_imports_call_helper() {
        // r0..r3 = undef; r4 = call r0 with this=r1 over window [r2, r2+2).
        let code = vec![
            load_undef(reg(0)),
            load_undef(reg(1)),
            load_undef(reg(2)),
            load_undef(reg(3)),
            Instruction::Call {
                dst: reg(4),
                callee: reg(0),
                this_value: reg(1),
                args_start: reg(2),
                arg_count: 2,
            },
            Instruction::Halt,
        ];
        let module = single(func(5, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::Call));
        assert_eq!(Helper::Call.external_index(), 9);
        let clif = function.clif.display().to_string();
        assert!(clif.contains("u1:9"), "call helper import missing:\n{clif}");
        assert!(
            clif.contains("(i64, i64, i64, i64, i32, i64) -> i32"),
            "call helper sig wrong:\n{clif}"
        );
        // args_start = r2 -> byte offset 16, computed with an iadd on handles.
        assert!(
            clif.contains("iadd"),
            "argument window pointer missing:\n{clif}"
        );
    }

    #[test]
    fn property_access_uses_string_key_constants() {
        let code = vec![
            Instruction::CreateObject { dst: reg(0) },
            Instruction::GetProperty {
                dst: reg(1),
                object: reg(0),
                key: ConstantId::new(0),
            },
            Instruction::Halt,
        ];
        let module = verified(
            vec![Constant::String("x".to_string())],
            vec![func(2, code, Vec::new())],
        );
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::CreateObject));
        assert!(function.helpers.contains(&Helper::GetProperty));
        let clif = function.clif.display().to_string();
        assert!(
            clif.contains("u1:6"),
            "get-property helper import missing:\n{clif}"
        );
    }

    #[test]
    fn import_and_define_function_are_lowered() {
        let code = vec![
            Instruction::DefineFunction {
                dst: reg(0),
                function: FunctionId::new(0),
            },
            Instruction::Import {
                dst: reg(1),
                specifier: ConstantId::new(0),
            },
            Instruction::Halt,
        ];
        let module = verified(
            vec![Constant::String("mod".to_string())],
            vec![func(2, code, Vec::new())],
        );
        let lowered = lower_module(&module, host_config()).expect("lowers");
        let function = &lowered.functions[0];
        assert!(function.helpers.contains(&Helper::DefineFunction));
        assert!(function.helpers.contains(&Helper::Import));
    }

    #[test]
    fn lowering_is_deterministic() {
        let code = vec![
            load_undef(reg(0)),
            Instruction::Binary {
                dst: reg(1),
                op: BinaryOp::Add,
                left: reg(0),
                right: reg(0),
            },
            Instruction::Jump { target: Pc::new(3) },
            Instruction::Halt,
        ];
        let make = || single(func(2, code.clone(), Vec::new()));
        let a = clif_of(&make());
        let b = clif_of(&make());
        assert_eq!(a, b);
    }

    #[test]
    fn unreachable_code_is_not_emitted() {
        // Jump over the Binary straight to Halt; the Binary is unreachable and
        // must not lower a helper.
        let code = vec![
            Instruction::Jump { target: Pc::new(2) },
            Instruction::Binary {
                dst: reg(0),
                op: BinaryOp::Add,
                left: reg(0),
                right: reg(0),
            },
            Instruction::Halt,
        ];
        let module = single(func(1, code, Vec::new()));
        let lowered = lower_module(&module, host_config()).expect("lowers");
        assert!(
            lowered.functions[0].helpers.is_empty(),
            "unreachable Binary lowered a helper"
        );
    }

    #[test]
    fn multiple_functions_get_distinct_symbols() {
        let functions = vec![
            func(0, vec![Instruction::Halt], Vec::new()),
            func(0, vec![Instruction::Halt], Vec::new()),
        ];
        let module = verified(Vec::new(), functions);
        let lowered = lower_module(&module, host_config()).expect("lowers");
        assert_eq!(lowered.functions.len(), 2);
        assert_eq!(lowered.functions[0].symbol, "bamts_fn_0");
        assert_eq!(lowered.functions[1].symbol, "bamts_fn_1");
        let name0 = lowered.functions[0].clif.display().to_string();
        assert!(name0.contains("u0:0"), "function 0 name wrong:\n{name0}");
        let name1 = lowered.functions[1].clif.display().to_string();
        assert!(name1.contains("u0:1"), "function 1 name wrong:\n{name1}");
    }

    #[test]
    fn innermost_handler_prefers_the_tightest_interval() {
        let outer = ExceptionHandler {
            start: Pc::new(0),
            end: Pc::new(10),
            handler: Pc::new(20),
            catch_register: reg(0),
        };
        let inner = ExceptionHandler {
            start: Pc::new(2),
            end: Pc::new(6),
            handler: Pc::new(30),
            catch_register: reg(1),
        };
        let handlers = [outer, inner];
        assert_eq!(
            innermost_handler(&handlers, 4).map(|h| h.handler),
            Some(Pc::new(30))
        );
        assert_eq!(
            innermost_handler(&handlers, 8).map(|h| h.handler),
            Some(Pc::new(20))
        );
        assert_eq!(innermost_handler(&handlers, 10), None);
    }

    #[test]
    fn non_64_bit_targets_are_rejected() {
        let config = TargetFrontendConfig {
            default_call_conv: CallConv::SystemV,
            pointer_width: {
                let flags = Flags::new(settings::builder());
                match isa::lookup_by_name("i686")
                    .ok()
                    .and_then(|b| b.finish(flags).ok())
                {
                    Some(target) => target.frontend_config().pointer_width,
                    None => return, // no 32-bit ISA compiled in; nothing to test
                }
            },
            page_size_align_log2: 12,
        };
        let module = single(func(0, vec![Instruction::Halt], Vec::new()));
        let error = lower_module(&module, config).expect_err("32-bit rejected");
        assert!(matches!(
            error,
            LowerError::UnsupportedPointerWidth { bits: 32 }
        ));
    }

    #[test]
    fn error_display_is_stable() {
        let width = LowerError::UnsupportedPointerWidth { bits: 32 };
        assert!(width.to_string().contains("64-bit"));
        let many = LowerError::TooManyFunctions { count: 200 };
        assert!(many.to_string().contains("200"));
        let slots = LowerError::RegisterFileTooLarge {
            function: FunctionId::new(1),
            register_count: 9,
        };
        assert!(slots.to_string().contains("function 1"));
        let sig = LowerError::EntrySignatureMismatch {
            function: FunctionId::new(2),
        };
        assert!(sig.to_string().contains("function 2"));
        let ir = LowerError::IrVerification {
            function: FunctionId::new(3),
            message: "boom".to_string(),
        };
        let text = ir.to_string();
        assert!(text.contains("function 3"));
        assert!(text.contains("boom"));
    }

    #[test]
    fn constant_pool_does_not_perturb_lowering() {
        let functions = vec![BytecodeFunction::new(
            Some(ConstantId::new(0)),
            0,
            0,
            FunctionFlags::default(),
            vec![Instruction::Halt],
            Vec::new(),
        )];
        let module = Module::new(
            vec![Constant::String("main".to_string())],
            functions,
            FunctionId::new(0),
        )
        .verify()
        .expect("verifies");
        let lowered = lower_module(&module, host_config()).expect("lowers");
        assert_eq!(lowered.functions[0].symbol, "bamts_fn_0");
    }
}
