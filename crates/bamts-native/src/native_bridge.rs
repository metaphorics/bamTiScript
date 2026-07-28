//! The native runtime bridge: the typed helper-call algebra, the exact 30
//! `bamts_*` C-ABI helper exports, the panic- and nesting-safe thread-local
//! [`NativeOps`] dispatch seam, and the feature-gated JIT and AOT linkage
//! surfaces.
//!
//! # Where the unsafe lives
//!
//! Generated CLIF (JIT) and linked object code (AOT) own control flow but call
//! back into this crate for every value/heap/host operation. This module is the
//! *only* place raw pointers from that generated code are turned into safe Rust:
//!
//! * The exported `bamts_*` wrappers ([`bamts_load_constant`] …) receive raw
//!   `*mut ShadowFrame` / `*mut Completion` from CLIF, validate them into a safe
//!   [`NativeFrame`], and dispatch to the current [`NativeOps`]. They never
//!   unwind across the C boundary: any panic, missing dispatcher, or invalid
//!   frame is turned into a [`CompletionTag::FatalTrap`].
//! * [`JitEntry`] (feature `jit-entry`) wraps a finalized `cranelift-jit`
//!   entry-point pointer, bound to its `JITModule` lifetime.
//! * [`linked_program`] (feature `aot-image`) reads the generated external
//!   `bamts_program_descriptor` image.
//!
//! The helper table below (variant order, symbols, and operand types) is the
//! export contract shared verbatim with `bamts_codegen::Helper`
//! (`crates/bamts-codegen/src/lib.rs`, `enum Helper` / `symbol` /
//! `external_index` / `param_types`). Any drift breaks linkage, so a
//! self-consistent parity test pins it (`bamts-codegen` depends on this crate
//! under `host-jit`, so a direct comparison would be a dependency cycle).

use core::mem::{align_of, size_of};

use std::cell::Cell;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{Completion, CompletionTag, ShadowFrame, Value};

// -- The helper algebra ------------------------------------------------------

/// The number of runtime helpers, `0..HELPER_COUNT`.
pub const HELPER_COUNT: u32 = 30;

/// A runtime helper, identified by its stable ABI index. The variant order is
/// the canonical `external_index` order (0..29) and is byte-identical to
/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_codegen::Helper`; [`NativeHelper::symbol`] returns the exact linker
/// symbol generated code resolves against.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NativeHelper {
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_load_constant` — index 0.
    LoadConstant = 0,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_unary` — index 1.
    Unary = 1,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_binary` — index 2.
    Binary = 2,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_create_object` — index 3.
    CreateObject = 3,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_create_array` — index 4.
    CreateArray = 4,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_create_closure` — index 5.
    CreateClosure = 5,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_get_property` — index 6.
    GetProperty = 6,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_set_property` — index 7.
    SetProperty = 7,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_delete_property` — index 8.
    DeleteProperty = 8,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_call` — index 9.
    Call = 9,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_construct` — index 10.
    Construct = 10,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_import` — index 11.
    Import = 11,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_truthy` — index 12 (returns `0`/`1`, never writes `out`).
    Truthy = 12,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_resume_value` — index 13.
    ResumeValue = 13,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_define_accessor` — index 14.
    DefineAccessor = 14,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_load_global` — index 15.
    LoadGlobal = 15,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_store_global` — index 16.
    StoreGlobal = 16,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_typeof_global` — index 17.
    TypeOfGlobal = 17,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_load_this` — index 18.
    LoadThis = 18,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_load_arguments` — index 19.
    LoadArguments = 19,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_load_new_target` — index 20.
    LoadNewTarget = 20,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_array_push` — index 21.
    ArrayPush = 21,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_array_extend` — index 22.
    ArrayExtend = 22,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_object_spread` — index 23.
    ObjectSpread = 23,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_set_prototype` — index 24.
    SetPrototype = 24,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_create_private_name` — index 25.
    CreatePrivateName = 25,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_create_regexp` — index 26.
    CreateRegExp = 26,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_get_iterator` — index 27.
    GetIterator = 27,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_iterator_next` — index 28 (writes two registers directly).
    IteratorNext = 28,
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_export` — index 29.
    Export = 29,
}

impl NativeHelper {
    /// The C symbol generated code links against. Byte-identical to
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_codegen::Helper::symbol`.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            NativeHelper::LoadConstant => "bamts_load_constant",
            NativeHelper::Unary => "bamts_unary",
            NativeHelper::Binary => "bamts_binary",
            NativeHelper::CreateObject => "bamts_create_object",
            NativeHelper::CreateArray => "bamts_create_array",
            NativeHelper::CreateClosure => "bamts_create_closure",
            NativeHelper::GetProperty => "bamts_get_property",
            NativeHelper::SetProperty => "bamts_set_property",
            NativeHelper::DeleteProperty => "bamts_delete_property",
            NativeHelper::Call => "bamts_call",
            NativeHelper::Construct => "bamts_construct",
            NativeHelper::Import => "bamts_import",
            NativeHelper::Truthy => "bamts_truthy",
            NativeHelper::ResumeValue => "bamts_resume_value",
            NativeHelper::DefineAccessor => "bamts_define_accessor",
            NativeHelper::LoadGlobal => "bamts_load_global",
            NativeHelper::StoreGlobal => "bamts_store_global",
            NativeHelper::TypeOfGlobal => "bamts_typeof_global",
            NativeHelper::LoadThis => "bamts_load_this",
            NativeHelper::LoadArguments => "bamts_load_arguments",
            NativeHelper::LoadNewTarget => "bamts_load_new_target",
            NativeHelper::ArrayPush => "bamts_array_push",
            NativeHelper::ArrayExtend => "bamts_array_extend",
            NativeHelper::ObjectSpread => "bamts_object_spread",
            NativeHelper::SetPrototype => "bamts_set_prototype",
            NativeHelper::CreatePrivateName => "bamts_create_private_name",
            NativeHelper::CreateRegExp => "bamts_create_regexp",
            NativeHelper::GetIterator => "bamts_get_iterator",
            NativeHelper::IteratorNext => "bamts_iterator_next",
            NativeHelper::Export => "bamts_export",
        }
    }

    /// The stable ABI index, `0..HELPER_COUNT`.
    #[inline]
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parses an ABI index, rejecting values outside `0..HELPER_COUNT`.
    #[must_use]
    pub const fn from_u32(index: u32) -> Option<NativeHelper> {
        match index {
            0 => Some(NativeHelper::LoadConstant),
            1 => Some(NativeHelper::Unary),
            2 => Some(NativeHelper::Binary),
            3 => Some(NativeHelper::CreateObject),
            4 => Some(NativeHelper::CreateArray),
            5 => Some(NativeHelper::CreateClosure),
            6 => Some(NativeHelper::GetProperty),
            7 => Some(NativeHelper::SetProperty),
            8 => Some(NativeHelper::DeleteProperty),
            9 => Some(NativeHelper::Call),
            10 => Some(NativeHelper::Construct),
            11 => Some(NativeHelper::Import),
            12 => Some(NativeHelper::Truthy),
            13 => Some(NativeHelper::ResumeValue),
            14 => Some(NativeHelper::DefineAccessor),
            15 => Some(NativeHelper::LoadGlobal),
            16 => Some(NativeHelper::StoreGlobal),
            17 => Some(NativeHelper::TypeOfGlobal),
            18 => Some(NativeHelper::LoadThis),
            19 => Some(NativeHelper::LoadArguments),
            20 => Some(NativeHelper::LoadNewTarget),
            21 => Some(NativeHelper::ArrayPush),
            22 => Some(NativeHelper::ArrayExtend),
            23 => Some(NativeHelper::ObjectSpread),
            24 => Some(NativeHelper::SetPrototype),
            25 => Some(NativeHelper::CreatePrivateName),
            26 => Some(NativeHelper::CreateRegExp),
            27 => Some(NativeHelper::GetIterator),
            28 => Some(NativeHelper::IteratorNext),
            29 => Some(NativeHelper::Export),
            _ => None,
        }
    }
}

/// A typed helper invocation. Runtime `Value`s carry their [`Value`] type;
/// operator selectors, string-constant ids, function/register indices, and
/// protocol kinds are the raw ABI `u32` selectors codegen passes (never
/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_bytecode` enums — this crate does not depend on the bytecode). The
/// implicit `frame` and completion `out` are supplied by [`NativeOps::dispatch`]
/// and the wrapper, not by the operands here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HelperCall {
    /// Materialize module constant `const_id`.
    LoadConstant { const_id: u32 },
    /// Apply unary operator selector `op` to `operand`.
    Unary { op: u32, operand: Value },
    /// Apply binary operator selector `op` to `left` and `right`.
    Binary { op: u32, left: Value, right: Value },
    /// A fresh empty object.
    CreateObject,
    /// A fresh empty array.
    CreateArray,
    /// A closure over `function_id` binding the `captures` array value.
    CreateClosure { function_id: u32, captures: Value },
    /// `object[key]`.
    GetProperty { object: Value, key: Value },
    /// `object[key] = value`.
    SetProperty {
        object: Value,
        key: Value,
        value: Value,
    },
    /// `delete object[key]`.
    DeleteProperty { object: Value, key: Value },
    /// Call `callee` with receiver `this_value` over the `arguments` array.
    Call {
        callee: Value,
        this_value: Value,
        arguments: Value,
    },
    /// Construct with `callee` over the `arguments` array.
    Construct { callee: Value, arguments: Value },
    /// Import the module named by string constant `specifier`.
    Import { specifier: u32 },
    /// ToBoolean on `value`. Routed to [`NativeOps::truthy`]; present here for a
    /// complete algebra but never delivered to [`NativeOps::dispatch`] in normal
    /// operation.
    Truthy { value: Value },
    /// The verified resumed value for the current frame.
    ResumeValue,
    /// Install a getter/setter (`kind` selector) under `key`.
    DefineAccessor {
        object: Value,
        key: Value,
        accessor: Value,
        kind: u32,
    },
    /// `globalThis[name]`.
    LoadGlobal { name: u32 },
    /// `globalThis[name] = value`.
    StoreGlobal { name: u32, value: Value },
    /// `typeof globalThis[name]`.
    TypeOfGlobal { name: u32 },
    /// The `this` binding.
    LoadThis,
    /// The `arguments` object.
    LoadArguments,
    /// `new.target`.
    LoadNewTarget,
    /// Append `value` to `array`.
    ArrayPush { array: Value, value: Value },
    /// Spread `iterable` onto the end of `array`.
    ArrayExtend { array: Value, iterable: Value },
    /// Copy own enumerable properties of `source` onto `target`.
    ObjectSpread { target: Value, source: Value },
    /// Set the `[[Prototype]]` of `object`.
    SetPrototype { object: Value, prototype: Value },
    /// A fresh private name described by string constant `description`.
    CreatePrivateName { description: u32 },
    /// A `RegExp` from string constants `pattern` and `flags`.
    CreateRegExp { pattern: u32, flags: u32 },
    /// Acquire an iterator over `src` using protocol `kind`.
    GetIterator { src: Value, kind: u32 },
    /// Advance `iterator`, writing the done flag into register `done_reg` and
    /// the produced value into register `value_reg` (both via the frame). On
    /// `Throw`, the thrown handle is the result value and neither register is
    /// written.
    IteratorNext {
        iterator: Value,
        done_reg: u32,
        value_reg: u32,
    },
    /// Export local value `src` under string constant `name`.
    Export { name: u32, src: Value },
}

impl HelperCall {
    /// The helper this call selects.
    #[must_use]
    pub const fn helper(&self) -> NativeHelper {
        match self {
            HelperCall::LoadConstant { .. } => NativeHelper::LoadConstant,
            HelperCall::Unary { .. } => NativeHelper::Unary,
            HelperCall::Binary { .. } => NativeHelper::Binary,
            HelperCall::CreateObject => NativeHelper::CreateObject,
            HelperCall::CreateArray => NativeHelper::CreateArray,
            HelperCall::CreateClosure { .. } => NativeHelper::CreateClosure,
            HelperCall::GetProperty { .. } => NativeHelper::GetProperty,
            HelperCall::SetProperty { .. } => NativeHelper::SetProperty,
            HelperCall::DeleteProperty { .. } => NativeHelper::DeleteProperty,
            HelperCall::Call { .. } => NativeHelper::Call,
            HelperCall::Construct { .. } => NativeHelper::Construct,
            HelperCall::Import { .. } => NativeHelper::Import,
            HelperCall::Truthy { .. } => NativeHelper::Truthy,
            HelperCall::ResumeValue => NativeHelper::ResumeValue,
            HelperCall::DefineAccessor { .. } => NativeHelper::DefineAccessor,
            HelperCall::LoadGlobal { .. } => NativeHelper::LoadGlobal,
            HelperCall::StoreGlobal { .. } => NativeHelper::StoreGlobal,
            HelperCall::TypeOfGlobal { .. } => NativeHelper::TypeOfGlobal,
            HelperCall::LoadThis => NativeHelper::LoadThis,
            HelperCall::LoadArguments => NativeHelper::LoadArguments,
            HelperCall::LoadNewTarget => NativeHelper::LoadNewTarget,
            HelperCall::ArrayPush { .. } => NativeHelper::ArrayPush,
            HelperCall::ArrayExtend { .. } => NativeHelper::ArrayExtend,
            HelperCall::ObjectSpread { .. } => NativeHelper::ObjectSpread,
            HelperCall::SetPrototype { .. } => NativeHelper::SetPrototype,
            HelperCall::CreatePrivateName { .. } => NativeHelper::CreatePrivateName,
            HelperCall::CreateRegExp { .. } => NativeHelper::CreateRegExp,
            HelperCall::GetIterator { .. } => NativeHelper::GetIterator,
            HelperCall::IteratorNext { .. } => NativeHelper::IteratorNext,
            HelperCall::Export { .. } => NativeHelper::Export,
        }
    }
}

/// The outcome of a completion helper: the ABI tag plus the completion value
/// its tag interprets (return value, thrown/yielded handle, or trap id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperResult {
    /// The completion class.
    pub tag: CompletionTag,
    /// The completion value; meaning fixed by `tag`.
    pub value: Value,
}

impl HelperResult {
    /// A `Normal` completion carrying `value`.
    #[inline]
    #[must_use]
    pub const fn normal(value: Value) -> HelperResult {
        HelperResult {
            tag: CompletionTag::Normal,
            value,
        }
    }

    /// A `Throw` completion carrying the rooted error handle `value`.
    #[inline]
    #[must_use]
    pub const fn throw(value: Value) -> HelperResult {
        HelperResult {
            tag: CompletionTag::Throw,
            value,
        }
    }
}

// -- Trap record ids ---------------------------------------------------------

/// `out.value` id written when a wrapper runs with no installed [`NativeOps`].
pub const TRAP_MISSING_NATIVE_OPS: u32 = 0x1000;
/// `out.value` id written when a wrapper receives an invalid frame pointer.
pub const TRAP_INVALID_FRAME: u32 = 0x1001;
/// `out.value` id written when the dispatcher panics (caught at the boundary).
pub const TRAP_PANIC: u32 = 0x1002;
/// `out.value` id written when a helper receives an out-of-range register index.
pub const TRAP_INVALID_REGISTER: u32 = 0x1003;
/// `out.value` id written when a native entry returns an unrecognized completion tag.
pub const TRAP_INVALID_COMPLETION_TAG: u32 = 0x1004;

// -- The safe frame view -----------------------------------------------------

/// A validated safe view of a [`ShadowFrame`] and its register (`handle`)
/// array. Constructed only inside the `bamts_*` wrappers, which reject a null,
/// misaligned, or malformed frame before dispatching.
pub struct NativeFrame<'a> {
    frame: &'a mut ShadowFrame,
    handles: &'a mut [Value],
}

impl<'a> NativeFrame<'a> {
    /// Builds a safe view from an already-borrowed frame and register slice.
    /// Returns `None` unless the frame's `handle_len` and `handles` metadata
    /// describe exactly `handles`. This lets runtime-owned execution paths use
    /// the same checked view without raw pointers or unsafe code.
    #[must_use]
    pub fn new(frame: &'a mut ShadowFrame, handles: &'a mut [Value]) -> Option<NativeFrame<'a>> {
        let len = u16::try_from(handles.len()).ok()?;
        if frame.handle_len != len {
            return None;
        }
        if !handles.is_empty() && !core::ptr::eq(frame.handles, handles.as_mut_ptr()) {
            return None;
        }
        Some(NativeFrame { frame, handles })
    }

    /// Validates a raw frame pointer into a safe view.
    ///
    /// # Safety
    ///
    /// `frame`, when non-null, must point to a live, unaliased [`ShadowFrame`]
    /// whose `handles` field addresses exactly `handle_len` initialized
    /// [`Value`]s (or is unused when `handle_len == 0`). Generated native code
    /// upholds this: it owns the frame for the synchronous duration of the
    /// helper call, and the register array is a distinct allocation from the
    /// 32-byte header. Returns `None` for a null or misaligned pointer.
    #[must_use]
    pub unsafe fn from_raw(frame: *mut ShadowFrame) -> Option<NativeFrame<'a>> {
        if frame.is_null() || !frame.addr().is_multiple_of(align_of::<ShadowFrame>()) {
            return None;
        }
        // Read only Copy fields through raw pointers before forming any mutable
        // reference. This lets us reject a handle range that aliases the header.
        let len = unsafe { core::ptr::addr_of!((*frame).handle_len).read() as usize };
        let handles_ptr = unsafe { core::ptr::addr_of!((*frame).handles).read() };
        if len != 0 {
            if handles_ptr.is_null() || !handles_ptr.addr().is_multiple_of(align_of::<Value>()) {
                return None;
            }
            let header_start = frame.addr();
            let header_end = header_start.checked_add(size_of::<ShadowFrame>())?;
            let handles_start = handles_ptr.addr();
            let handles_end = handles_start.checked_add(len.checked_mul(size_of::<Value>())?)?;
            if handles_start < header_end && header_start < handles_end {
                return None;
            }
        }
        // SAFETY: the caller contract guarantees the validated raw frame is live
        // and unaliased; the address-range check above proves its handle storage
        // cannot overlap the header before these mutable references are formed.
        let header: &'a mut ShadowFrame = unsafe { &mut *frame };
        let handles: &'a mut [Value] = if len == 0 {
            &mut []
        } else {
            // SAFETY: checked above for alignment, range arithmetic, and
            // non-overlap; the caller contract guarantees initialized Values.
            unsafe { core::slice::from_raw_parts_mut(handles_ptr, len) }
        };
        Some(NativeFrame {
            frame: header,
            handles,
        })
    }

    /// The number of live registers (`ShadowFrame::handle_len`).
    #[inline]
    #[must_use]
    pub fn handle_len(&self) -> u32 {
        u32::from(self.frame.handle_len)
    }

    /// The dense module id of the executing function.
    #[inline]
    #[must_use]
    pub fn module_id(&self) -> u32 {
        self.frame.module_id
    }

    /// The current bytecode program counter / resume token.
    #[inline]
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.frame.bytecode_pc
    }

    /// Stores a resume token into the frame (yield path).
    #[inline]
    pub fn set_resume(&mut self, token: u32) {
        self.frame.bytecode_pc = token;
    }

    /// The register array.
    #[inline]
    #[must_use]
    pub fn registers(&self) -> &[Value] {
        self.handles
    }

    /// The register array, mutably.
    #[inline]
    #[must_use]
    pub fn registers_mut(&mut self) -> &mut [Value] {
        self.handles
    }

    /// Register `index`. Panics if out of range; the wrapper turns the panic
    /// into a [`CompletionTag::FatalTrap`]. Use [`NativeFrame::try_register`] to
    /// branch instead.
    #[inline]
    #[must_use]
    pub fn register(&self, index: u32) -> Value {
        self.handles[index as usize]
    }

    /// Sets register `index`. Panics if out of range (see [`NativeFrame::register`]).
    #[inline]
    pub fn set_register(&mut self, index: u32, value: Value) {
        self.handles[index as usize] = value;
    }

    /// Register `index`, or `None` when out of range.
    #[inline]
    #[must_use]
    pub fn try_register(&self, index: u32) -> Option<Value> {
        self.handles.get(index as usize).copied()
    }

    /// Sets register `index`, returning `false` when out of range.
    #[inline]
    pub fn try_set_register(&mut self, index: u32, value: Value) -> bool {
        match self.handles.get_mut(index as usize) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }

    /// The caller's frame, or null at the base of the stack.
    #[inline]
    #[must_use]
    pub fn previous(&self) -> *mut ShadowFrame {
        self.frame.previous
    }
}
/// Validates a raw frame pointer into a safe view.
///
/// # Safety
///
/// `frame` must be either null or a live, unaliased [`ShadowFrame`] pointer
/// owned by the caller for the synchronous duration of the call, with
/// `handle_len` initialized [`Value`]s when non-null. This is the same
/// contract as [`NativeFrame::from_raw`].
unsafe fn frame_from_raw<'a>(frame: *mut ShadowFrame) -> Option<NativeFrame<'a>> {
    // SAFETY: forwarded to `NativeFrame::from_raw`, which validates null and
    // alignment before dereferencing; the caller upholds the lifetime and
    // aliasing contract described above.
    unsafe { NativeFrame::from_raw(frame) }
}

// -- The dispatch seam -------------------------------------------------------

/// The runtime semantic engine the exported helpers dispatch into. Implemented
/// by `bamts_runtime`'s native engine; installed for the current thread with
/// [`with_native_ops`].
///
/// Methods take `&self` because dispatch is **re-entrant**: a `Call`,
/// `Construct`, or `CreateClosure` may re-enter native code that calls another
/// helper, which dispatches back into the same instance on the same thread.
/// Shared `&self` reborrows alias soundly, so the outer `dispatch` may resume
/// touching `self` after the nested call returns — e.g. to pop an activation
/// record or record a result. The engine therefore holds its mutable state
/// behind interior mutability (`Cell`/`RefCell`/`UnsafeCell`); the one
/// discipline is to never hold a `RefCell` borrow guard across a nested native
/// re-entry (the re-entrant borrow would panic). Nested execution always uses a
/// distinct child [`ShadowFrame`], so the outer `frame` borrow never aliases it.
pub trait NativeOps {
    /// The total ToBoolean coercion. Never throws.
    fn truthy(&self, frame: &mut NativeFrame<'_>, value: Value) -> bool;

    /// Executes one completion helper, writing any register side effects through
    /// `frame` and returning the completion.
    fn dispatch(&self, frame: &mut NativeFrame<'_>, call: HelperCall) -> HelperResult;
}

type ErasedOps = *const (dyn NativeOps + 'static);

thread_local! {
    /// The [`NativeOps`] active on this thread, or `None`. Holds an erased raw
    /// pointer valid only for the duration of the [`with_native_ops`] scope that
    /// installed it.
    static CURRENT_OPS: Cell<Option<ErasedOps>> = const { Cell::new(None) };
}

/// Installs `ops` as the current thread's dispatcher for the duration of
/// `body`, then restores the previous dispatcher.
///
/// Nesting-safe: an inner `with_native_ops` saves and restores the outer
/// dispatcher. Panic-safe: the previous dispatcher is restored even if `body`
/// unwinds (the restore runs in a guard's `Drop`).
pub fn with_native_ops<R>(ops: &mut dyn NativeOps, body: impl FnOnce() -> R) -> R {
    // `ops` is taken uniquely to guarantee the caller owns the engine while it
    // is installed, but the dispatcher is invoked through shared `&self`
    // reborrows (see `NativeOps`), so it is stored as a shared raw pointer;
    // re-entrant native calls then form further shared reborrows that alias
    // soundly.
    let ptr: *const dyn NativeOps = ops;
    // SAFETY: `ptr` and `erased` have the identical fat-pointer representation,
    // alignment, initialization, and provenance; the transmute changes only the
    // erased lifetime metadata, not the data or vtable addresses. `ptr` stays
    // valid for all of `body`, and the guard restores the previous slot before
    // return/unwind, so the erased pointer is never dereferenced outside that
    // lifetime. No bounds are involved.
    let erased: ErasedOps = unsafe { core::mem::transmute::<*const dyn NativeOps, ErasedOps>(ptr) };
    let previous = CURRENT_OPS.with(|slot| slot.replace(Some(erased)));
    let _guard = OpsGuard { previous };
    body()
}

/// Restores the previous [`CURRENT_OPS`] value on scope exit, including unwind.
struct OpsGuard {
    previous: Option<ErasedOps>,
}

impl Drop for OpsGuard {
    fn drop(&mut self) {
        CURRENT_OPS.with(|slot| slot.set(self.previous));
    }
}

/// Runs `f` with the current thread's dispatcher, or returns `None` if none is
/// installed.
fn with_current_ops<R>(f: impl FnOnce(&dyn NativeOps) -> R) -> Option<R> {
    let ptr = CURRENT_OPS.with(|slot| slot.get())?;
    // SAFETY: `ptr` was installed by an enclosing `with_native_ops` and remains
    // valid/aligned/initialized for that scope, preserving the trait object's
    // data/vtable provenance. It is dereferenced only as a shared `&dyn`; any
    // number of these may coexist across re-entrant helper calls, so aliasing is
    // sound. The helper returns before the installation scope ends.
    let ops: &dyn NativeOps = unsafe { &*ptr };
    Some(f(ops))
}

/// The internal outcome of a completion helper before it is written to `out`.
enum HelperOutcome {
    Done(HelperResult),
    Trap(u32),
}

/// The shared body of every completion `bamts_*` wrapper: validate the frame,
/// find the dispatcher, build the [`HelperCall`], write the completion, and
/// return the exact tag discriminant. Never unwinds; every failure becomes a
/// `FatalTrap`.
fn run_completion_helper(
    frame: *mut ShadowFrame,
    out: *mut Completion,
    build: impl FnOnce(&mut NativeFrame<'_>, &dyn NativeOps) -> HelperResult,
) -> u32 {
    // There is no writable completion slot on this path. Return the exact fatal
    // tag without dereferencing `out`; generated code treats the tag as control
    // flow and never reads `out.value` after a FatalTrap.
    if out.is_null() || !out.addr().is_multiple_of(align_of::<Completion>()) {
        return CompletionTag::FatalTrap.as_u32();
    }

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller (generated native code) passes a frame it owns for
        // this synchronous call; `frame_from_raw` rejects null/misaligned pointers.
        let mut native_frame = match unsafe { frame_from_raw(frame) } {
            Some(view) => view,
            None => return HelperOutcome::Trap(TRAP_INVALID_FRAME),
        };
        match with_current_ops(|ops| build(&mut native_frame, ops)) {
            Some(result) => HelperOutcome::Done(result),
            None => HelperOutcome::Trap(TRAP_MISSING_NATIVE_OPS),
        }
    }));

    let (tag, value) = match outcome {
        Ok(HelperOutcome::Done(result)) => (result.tag, result.value),
        Ok(HelperOutcome::Trap(id)) => (CompletionTag::FatalTrap, Value::int32(id)),
        Err(_) => (CompletionTag::FatalTrap, Value::int32(TRAP_PANIC)),
    };

    // SAFETY: `out` is non-null and aligned (checked above); generated native
    // code passes a valid, writable Completion out-parameter it owns for this
    // synchronous call.
    unsafe { core::ptr::write(out, Completion::new(value)) };
    tag.as_u32()
}

/// Dispatches `call` through the current dispatcher inside a validated frame.
/// Helper for wrappers whose `HelperCall` needs no frame data to build.
#[inline]
fn dispatch_simple(frame: *mut ShadowFrame, out: *mut Completion, call: HelperCall) -> u32 {
    run_completion_helper(frame, out, |native_frame, ops| {
        ops.dispatch(native_frame, call)
    })
}

// -- The exact 30 exported C-ABI helpers -------------------------------------
//
// Parameter order and widths mirror `bamts_codegen::Helper::param_types`
// exactly: `frame` (pointer) first, `out` (pointer) last, runtime `Value`s as
// `u64`, and selectors/indices as `u32`. `bamts_truthy` is the sole exception:
// no `out`, returns `0`/`1`.

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_load_constant(frame, const_id, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_load_constant(
    frame: *mut ShadowFrame,
    const_id: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::LoadConstant { const_id })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_unary(frame, op, operand, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_unary(
    frame: *mut ShadowFrame,
    op: u32,
    operand: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::Unary {
            op,
            operand: Value::from_bits(operand),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_binary(frame, op, left, right, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_binary(
    frame: *mut ShadowFrame,
    op: u32,
    left: u64,
    right: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::Binary {
            op,
            left: Value::from_bits(left),
            right: Value::from_bits(right),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_create_object(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_create_object(frame: *mut ShadowFrame, out: *mut Completion) -> u32 {
    dispatch_simple(frame, out, HelperCall::CreateObject)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_create_array(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_create_array(frame: *mut ShadowFrame, out: *mut Completion) -> u32 {
    dispatch_simple(frame, out, HelperCall::CreateArray)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_create_closure(frame, function_id, captures, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_create_closure(
    frame: *mut ShadowFrame,
    function_id: u32,
    captures: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::CreateClosure {
            function_id,
            captures: Value::from_bits(captures),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_get_property(frame, object, key, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_get_property(
    frame: *mut ShadowFrame,
    object: u64,
    key: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::GetProperty {
            object: Value::from_bits(object),
            key: Value::from_bits(key),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_set_property(frame, object, key, value, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_set_property(
    frame: *mut ShadowFrame,
    object: u64,
    key: u64,
    value: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::SetProperty {
            object: Value::from_bits(object),
            key: Value::from_bits(key),
            value: Value::from_bits(value),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_delete_property(frame, object, key, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_delete_property(
    frame: *mut ShadowFrame,
    object: u64,
    key: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::DeleteProperty {
            object: Value::from_bits(object),
            key: Value::from_bits(key),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_call(frame, callee, this, arguments, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_call(
    frame: *mut ShadowFrame,
    callee: u64,
    this_value: u64,
    arguments: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::Call {
            callee: Value::from_bits(callee),
            this_value: Value::from_bits(this_value),
            arguments: Value::from_bits(arguments),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_construct(frame, callee, arguments, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_construct(
    frame: *mut ShadowFrame,
    callee: u64,
    arguments: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::Construct {
            callee: Value::from_bits(callee),
            arguments: Value::from_bits(arguments),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_import(frame, specifier, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_import(
    frame: *mut ShadowFrame,
    specifier: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::Import { specifier })
}

/// Validates `frame` and runs the tagless truthy helper for `value`.
///
/// Returns `None` when the frame is invalid or no dispatcher is installed.
fn truthy_from_raw(frame: *mut ShadowFrame, value: u64) -> Option<bool> {
    // SAFETY: generated native code passes a frame it owns for this
    // synchronous call; `frame_from_raw` rejects null or misaligned pointers
    // before any dereference.
    let mut native_frame = unsafe { frame_from_raw(frame) }?;
    with_current_ops(|ops| ops.truthy(&mut native_frame, Value::from_bits(value)))
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_truthy(frame, value) -> u32`. Total; never writes `out` (there is
/// none) and never throws. Returns `0` on an invalid frame or missing
/// dispatcher, the only channel available to a tagless helper.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_truthy(frame: *mut ShadowFrame, value: u64) -> u32 {
    let outcome = catch_unwind(AssertUnwindSafe(|| truthy_from_raw(frame, value)));
    match outcome {
        Ok(Some(true)) => 1,
        _ => 0,
    }
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_resume_value(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_resume_value(frame: *mut ShadowFrame, out: *mut Completion) -> u32 {
    dispatch_simple(frame, out, HelperCall::ResumeValue)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_define_accessor(frame, object, key, accessor, kind, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_define_accessor(
    frame: *mut ShadowFrame,
    object: u64,
    key: u64,
    accessor: u64,
    kind: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::DefineAccessor {
            object: Value::from_bits(object),
            key: Value::from_bits(key),
            accessor: Value::from_bits(accessor),
            kind,
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_load_global(frame, name, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_load_global(
    frame: *mut ShadowFrame,
    name: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::LoadGlobal { name })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_store_global(frame, name, value, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_store_global(
    frame: *mut ShadowFrame,
    name: u32,
    value: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::StoreGlobal {
            name,
            value: Value::from_bits(value),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_typeof_global(frame, name, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_typeof_global(
    frame: *mut ShadowFrame,
    name: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::TypeOfGlobal { name })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_load_this(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_load_this(frame: *mut ShadowFrame, out: *mut Completion) -> u32 {
    dispatch_simple(frame, out, HelperCall::LoadThis)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_load_arguments(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_load_arguments(
    frame: *mut ShadowFrame,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::LoadArguments)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_load_new_target(frame, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_load_new_target(
    frame: *mut ShadowFrame,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::LoadNewTarget)
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_array_push(frame, array, value, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_array_push(
    frame: *mut ShadowFrame,
    array: u64,
    value: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::ArrayPush {
            array: Value::from_bits(array),
            value: Value::from_bits(value),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_array_extend(frame, array, iterable, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_array_extend(
    frame: *mut ShadowFrame,
    array: u64,
    iterable: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::ArrayExtend {
            array: Value::from_bits(array),
            iterable: Value::from_bits(iterable),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_object_spread(frame, target, source, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_object_spread(
    frame: *mut ShadowFrame,
    target: u64,
    source: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::ObjectSpread {
            target: Value::from_bits(target),
            source: Value::from_bits(source),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_set_prototype(frame, object, prototype, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_set_prototype(
    frame: *mut ShadowFrame,
    object: u64,
    prototype: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::SetPrototype {
            object: Value::from_bits(object),
            prototype: Value::from_bits(prototype),
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_create_private_name(frame, description, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_create_private_name(
    frame: *mut ShadowFrame,
    description: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::CreatePrivateName { description })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_create_regexp(frame, pattern, flags, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_create_regexp(
    frame: *mut ShadowFrame,
    pattern: u32,
    flags: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(frame, out, HelperCall::CreateRegExp { pattern, flags })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_get_iterator(frame, src, kind, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_get_iterator(
    frame: *mut ShadowFrame,
    src: u64,
    kind: u32,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::GetIterator {
            src: Value::from_bits(src),
            kind,
        },
    )
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_iterator_next(frame, iterator, done_reg, value_reg, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_iterator_next(
    frame: *mut ShadowFrame,
    iterator: u64,
    done_reg: u32,
    value_reg: u32,
    out: *mut Completion,
) -> u32 {
    run_completion_helper(frame, out, |native_frame, ops| {
        if done_reg >= native_frame.handle_len() || value_reg >= native_frame.handle_len() {
            return HelperResult {
                tag: CompletionTag::FatalTrap,
                value: Value::int32(TRAP_INVALID_REGISTER),
            };
        }
        ops.dispatch(
            native_frame,
            HelperCall::IteratorNext {
                iterator: Value::from_bits(iterator),
                done_reg,
                value_reg,
            },
        )
    })
}

/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_export(frame, name, src, out)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bamts_export(
    frame: *mut ShadowFrame,
    name: u32,
    src: u64,
    out: *mut Completion,
) -> u32 {
    dispatch_simple(
        frame,
        out,
        HelperCall::Export {
            name,
            src: Value::from_bits(src),
        },
    )
}

// Compile-time signature parity with `bamts_codegen::Helper::param_types`. Each
// `const _` binds a `bamts_*` export to its exact ABI signature (every runtime
// `Value` is a 64-bit scalar, so `u64`; selectors/indices are `u32`). Any drift
// in codegen's helper parameter order, count, or width stops this crate from
// compiling, so the JIT/AOT linker can never silently mismatch a helper.
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, *mut Completion) -> u32 = bamts_load_constant; // 0
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u64, *mut Completion) -> u32 = bamts_unary; // 1
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u64, u64, *mut Completion) -> u32 =
    bamts_binary; // 2
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_create_object; // 3
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_create_array; // 4
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u64, *mut Completion) -> u32 =
    bamts_create_closure; // 5
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_get_property; // 6
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, u64, *mut Completion) -> u32 =
    bamts_set_property; // 7
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_delete_property; // 8
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, u64, *mut Completion) -> u32 = bamts_call; // 9
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 = bamts_construct; // 10
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, *mut Completion) -> u32 = bamts_import; // 11
const _: unsafe extern "C" fn(*mut ShadowFrame, u64) -> u32 = bamts_truthy; // 12 (no out)
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_resume_value; // 13
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, u64, u32, *mut Completion) -> u32 =
    bamts_define_accessor; // 14
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, *mut Completion) -> u32 = bamts_load_global; // 15
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u64, *mut Completion) -> u32 =
    bamts_store_global; // 16
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, *mut Completion) -> u32 = bamts_typeof_global; // 17
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_load_this; // 18
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_load_arguments; // 19
const _: unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32 = bamts_load_new_target; // 20
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_array_push; // 21
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_array_extend; // 22
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_object_spread; // 23
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u64, *mut Completion) -> u32 =
    bamts_set_prototype; // 24
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, *mut Completion) -> u32 =
    bamts_create_private_name; // 25
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u32, *mut Completion) -> u32 =
    bamts_create_regexp; // 26
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u32, *mut Completion) -> u32 =
    bamts_get_iterator; // 27
const _: unsafe extern "C" fn(*mut ShadowFrame, u64, u32, u32, *mut Completion) -> u32 =
    bamts_iterator_next; // 28
const _: unsafe extern "C" fn(*mut ShadowFrame, u32, u64, *mut Completion) -> u32 = bamts_export; // 29

// -- Native entry invocation seam --------------------------------------------

/// A finalized native entry point: `extern "C" fn(frame, out) -> tag`.
pub type NativeEntryFn = unsafe extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32;

/// A non-self-referential seam for invoking a compiled native entry by its
/// `(module_id, function_id)` identity. A JIT backend implements this over its
/// `JITModule`; a linked AOT image ([`LinkedProgram`]) implements it over its
/// unit table. The runtime engine stores `&dyn NativeEntryTable` and routes
/// nested `CreateClosure` re-entry through it.
pub trait NativeEntryTable {
    /// The exact canonical [`bamts_bytecode::Program::encode`] bytes compiled into these entries.
    ///
    /// Callers must compare these bytes with the supplied program before any native entry runs.
    fn program_bytes(&self) -> &[u8];

    /// Invokes the entry for `(module_id, function_id)`, returning its completion tag.
    fn invoke(
        &self,
        module_id: u32,
        function_id: u32,
        frame: &mut ShadowFrame,
        out: &mut Completion,
    ) -> Result<CompletionTag, AbiError>;
}

/// Calls a raw native entry with unique references, mapping the raw `u32` result
/// to a [`CompletionTag`] (an out-of-range tag becomes `FatalTrap`).
///
/// # Safety
///
/// `entry` must be a finalized native entry with the exact
/// `extern "C" fn(*mut ShadowFrame, *mut Completion) -> u32` ABI, and its code
/// must remain mapped for the duration of the call.
unsafe fn call_native_entry(
    entry: NativeEntryFn,
    frame: &mut ShadowFrame,
    out: &mut Completion,
) -> CompletionTag {
    // SAFETY: `entry` upholds the native-entry ABI (caller contract); `frame`
    // and `out` are unique valid references, so the raw pointers are valid and
    // unaliased for the synchronous call.
    let raw = unsafe { entry(frame as *mut ShadowFrame, out as *mut Completion) };
    match CompletionTag::from_u32(raw) {
        Some(tag) => tag,
        None => {
            *out = Completion::new(Value::int32(TRAP_INVALID_COMPLETION_TAG));
            CompletionTag::FatalTrap
        }
    }
}

// -- AOT image linkage (types + validator are always available) --------------

/// The little-endian image magic, `b"BMTSAOT1"`.
pub const AOT_MAGIC: u64 = u64::from_le_bytes(*b"BMTSAOT1");
/// The supported AOT image ABI version.
pub const AOT_ABI_VERSION: u32 = 2;

/// One compiled function in a linked AOT image.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UnitDescriptor {
    /// The bytecode function id this entry implements.
    pub function_id: u32,
    /// The bytecode module id containing `function_id`.
    pub module_id: u32,
    /// The finalized native entry.
    pub entry: NativeEntryFn,
}

/// The C-layout header of a linked AOT program image, exported by generated
/// code as the external symbol `bamts_program_descriptor`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProgramDescriptor {
    /// [`AOT_MAGIC`].
    pub magic: u64,
    /// [`AOT_ABI_VERSION`].
    pub abi_version: u32,
    /// Reserved flags; must be zero.
    pub flags: u32,
    /// The embedded verified bytecode image.
    pub bytecode: *const u8,
    /// The length of `bytecode` in bytes.
    pub bytecode_len: usize,
    /// The unit table.
    pub units: *const UnitDescriptor,
    /// The number of units.
    pub unit_count: usize,
    /// The entry function id (must appear in `units` with `entry_module`).
    pub entry_function: u32,
    /// The entry module id.
    pub entry_module: u32,
}

/// A validated, borrow-checked view of a linked AOT image. The bytecode is
/// carried opaquely (native code never decodes it); reconstruction/verification
/// is the runtime's job.
pub struct LinkedProgram<'a> {
    bytecode: &'a [u8],
    units: &'a [UnitDescriptor],
    entry_module: u32,
    entry_function: u32,
}

impl<'a> LinkedProgram<'a> {
    /// Validates a program descriptor into a borrowed linked view.
    ///
    /// Checks magic, ABI version, zeroed flags, non-null and non-empty bytecode
    /// and unit tables, overflow-safe slice extents, unit identities sorted and
    /// unique by `(module_id, function_id)`, and that the tuple entry is present.
    /// Layout is only defined on 64-bit targets; other widths yield
    /// [`AbiError::UnsupportedPointerWidth`].
    ///
    /// # Safety
    ///
    /// `descriptor`'s `bytecode`/`units` pointers must address `bytecode_len`
    /// bytes / `unit_count` [`UnitDescriptor`]s that remain valid and immutable
    /// for `'a`. The generated `bamts_program_descriptor` satisfies this for
    /// `'static`; a caller validating a synthetic descriptor guarantees it.
    pub unsafe fn from_descriptor(
        descriptor: &'a ProgramDescriptor,
    ) -> Result<LinkedProgram<'a>, AbiError> {
        if size_of::<usize>() != 8 {
            return Err(AbiError::UnsupportedPointerWidth {
                bits: usize::BITS as u16,
            });
        }
        if descriptor.magic != AOT_MAGIC {
            return Err(AbiError::BadMagic {
                found: descriptor.magic,
            });
        }
        if descriptor.abi_version != AOT_ABI_VERSION {
            return Err(AbiError::UnsupportedAbiVersion {
                found: descriptor.abi_version,
            });
        }
        if descriptor.flags != 0 {
            return Err(AbiError::NonZeroFlags {
                flags: descriptor.flags,
            });
        }

        if descriptor.bytecode_len == 0 {
            return Err(AbiError::EmptyBytecode);
        }
        if descriptor.bytecode.is_null() {
            return Err(AbiError::NullBytecode);
        }
        if descriptor.bytecode_len > isize::MAX as usize {
            return Err(AbiError::LengthOverflow);
        }

        if descriptor.unit_count == 0 {
            return Err(AbiError::EmptyUnits);
        }
        if descriptor.units.is_null() {
            return Err(AbiError::NullUnits);
        }
        let unit_bytes = descriptor
            .unit_count
            .checked_mul(size_of::<UnitDescriptor>())
            .ok_or(AbiError::LengthOverflow)?;
        if unit_bytes > isize::MAX as usize {
            return Err(AbiError::LengthOverflow);
        }

        // SAFETY: `bytecode` is non-null and its length is bounded by
        // `isize::MAX`; the caller contract guarantees the memory stays valid
        // and immutable for `'a`.
        let bytecode =
            unsafe { core::slice::from_raw_parts(descriptor.bytecode, descriptor.bytecode_len) };
        // SAFETY: `units` is non-null and `unit_count * size_of::<UnitDescriptor>()`
        // is bounded by `isize::MAX`; the caller contract guarantees the memory
        // stays valid and immutable for `'a`.
        let units = unsafe { core::slice::from_raw_parts(descriptor.units, descriptor.unit_count) };

        let mut entry_present = false;
        let mut previous = None;
        for unit in units {
            let identity = (unit.module_id, unit.function_id);
            if identity == (descriptor.entry_module, descriptor.entry_function) {
                entry_present = true;
            }
            if let Some((previous_module_id, previous_function_id)) = previous {
                match identity.cmp(&(previous_module_id, previous_function_id)) {
                    core::cmp::Ordering::Less => {
                        return Err(AbiError::UnsortedUnits {
                            previous_module_id,
                            previous_function_id,
                            module_id: unit.module_id,
                            function_id: unit.function_id,
                        });
                    }
                    core::cmp::Ordering::Equal => {
                        return Err(AbiError::DuplicateFunction {
                            module_id: unit.module_id,
                            function_id: unit.function_id,
                        });
                    }
                    core::cmp::Ordering::Greater => {}
                }
            }
            previous = Some(identity);
        }
        if !entry_present {
            return Err(AbiError::EntryFunctionMissing {
                module_id: descriptor.entry_module,
                function_id: descriptor.entry_function,
            });
        }

        Ok(LinkedProgram {
            bytecode,
            units,
            entry_module: descriptor.entry_module,
            entry_function: descriptor.entry_function,
        })
    }

    /// The embedded canonical [`bamts_bytecode::Program::encode`] bytes (opaque to native code).
    #[inline]
    #[must_use]
    pub fn bytecode(&self) -> &'a [u8] {
        self.bytecode
    }

    /// The validated unit table.
    #[inline]
    #[must_use]
    pub fn units(&self) -> &'a [UnitDescriptor] {
        self.units
    }

    /// The entry module id.
    #[inline]
    #[must_use]
    pub fn entry_module(&self) -> u32 {
        self.entry_module
    }

    /// The entry function id.
    #[inline]
    #[must_use]
    pub fn entry_function(&self) -> u32 {
        self.entry_function
    }

    /// The unit for `(module_id, function_id)`, if present.
    #[must_use]
    pub fn unit(&self, module_id: u32, function_id: u32) -> Option<&'a UnitDescriptor> {
        self.units
            .binary_search_by_key(&(module_id, function_id), |unit| {
                (unit.module_id, unit.function_id)
            })
            .ok()
            .map(|index| &self.units[index])
    }
}

impl NativeEntryTable for LinkedProgram<'_> {
    fn program_bytes(&self) -> &[u8] {
        self.bytecode
    }

    fn invoke(
        &self,
        module_id: u32,
        function_id: u32,
        frame: &mut ShadowFrame,
        out: &mut Completion,
    ) -> Result<CompletionTag, AbiError> {
        let unit = self
            .unit(module_id, function_id)
            .ok_or(AbiError::UnknownFunction {
                module_id,
                function_id,
            })?;
        // SAFETY: `unit.entry` is a finalized native entry installed by the AOT
        // image, upholding the native-entry ABI; its code stays mapped for the
        // program's lifetime.
        Ok(unsafe { call_native_entry(unit.entry, frame, out) })
    }
}

/// Compile-time AOT layout assertions (64-bit targets only, per the ABI).
#[cfg(target_pointer_width = "64")]
const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    // UnitDescriptor: { u32, u32, fn ptr } => 16 bytes, 8-aligned.
    assert!(size_of::<UnitDescriptor>() == 16);
    assert!(align_of::<UnitDescriptor>() == 8);
    assert!(offset_of!(UnitDescriptor, function_id) == 0);
    assert!(offset_of!(UnitDescriptor, module_id) == 4);
    assert!(offset_of!(UnitDescriptor, entry) == 8);

    // ProgramDescriptor: 56 bytes, 8-aligned, fields at fixed offsets.
    assert!(size_of::<ProgramDescriptor>() == 56);
    assert!(align_of::<ProgramDescriptor>() == 8);
    assert!(offset_of!(ProgramDescriptor, magic) == 0);
    assert!(offset_of!(ProgramDescriptor, abi_version) == 8);
    assert!(offset_of!(ProgramDescriptor, flags) == 12);
    assert!(offset_of!(ProgramDescriptor, bytecode) == 16);
    assert!(offset_of!(ProgramDescriptor, bytecode_len) == 24);
    assert!(offset_of!(ProgramDescriptor, units) == 32);
    assert!(offset_of!(ProgramDescriptor, unit_count) == 40);
    assert!(offset_of!(ProgramDescriptor, entry_function) == 48);
    assert!(offset_of!(ProgramDescriptor, entry_module) == 52);
};

/// A typed AOT/entry-linkage failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiError {
    /// The target pointer width is not 64-bit.
    UnsupportedPointerWidth {
        /// The offending width in bits.
        bits: u16,
    },
    /// The descriptor magic did not match [`AOT_MAGIC`].
    BadMagic {
        /// The observed magic.
        found: u64,
    },
    /// The descriptor ABI version is not [`AOT_ABI_VERSION`].
    UnsupportedAbiVersion {
        /// The observed version.
        found: u32,
    },
    /// A reserved `flags` field was non-zero.
    NonZeroFlags {
        /// The observed flags.
        flags: u32,
    },
    /// The bytecode pointer was null with a non-zero length.
    NullBytecode,
    /// The bytecode image was empty.
    EmptyBytecode,
    /// The unit pointer was null with a non-zero count.
    NullUnits,
    /// The unit table was empty.
    EmptyUnits,
    /// A slice extent overflowed `isize::MAX`.
    LengthOverflow,
    /// Unit identities are not sorted by `(module_id, function_id)`.
    UnsortedUnits {
        /// The preceding module id.
        previous_module_id: u32,
        /// The preceding function id.
        previous_function_id: u32,
        /// The out-of-order module id.
        module_id: u32,
        /// The out-of-order function id.
        function_id: u32,
    },
    /// Two units shared a `(module_id, function_id)` identity.
    DuplicateFunction {
        /// The duplicated module id.
        module_id: u32,
        /// The duplicated function id.
        function_id: u32,
    },
    /// The declared tuple entry was absent from the unit table.
    EntryFunctionMissing {
        /// The missing module id.
        module_id: u32,
        /// The missing function id.
        function_id: u32,
    },
    /// [`NativeEntryTable::invoke`] was asked for an unknown function identity.
    UnknownFunction {
        /// The requested module id.
        module_id: u32,
        /// The requested function id.
        function_id: u32,
    },
}

impl fmt::Display for AbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AbiError::UnsupportedPointerWidth { bits } => {
                write!(f, "AOT image requires a 64-bit target, not {bits}-bit")
            }
            AbiError::BadMagic { found } => {
                write!(f, "AOT image magic {found:#018x} != {AOT_MAGIC:#018x}")
            }
            AbiError::UnsupportedAbiVersion { found } => {
                write!(f, "AOT image ABI version {found} != {AOT_ABI_VERSION}")
            }
            AbiError::NonZeroFlags { flags } => {
                write!(f, "AOT image flags {flags:#x} must be zero")
            }
            AbiError::NullBytecode => f.write_str("AOT image bytecode pointer is null"),
            AbiError::EmptyBytecode => f.write_str("AOT image bytecode is empty"),
            AbiError::NullUnits => f.write_str("AOT image unit pointer is null"),
            AbiError::EmptyUnits => f.write_str("AOT image unit table is empty"),
            AbiError::LengthOverflow => f.write_str("AOT image slice extent overflows isize::MAX"),
            AbiError::UnsortedUnits {
                previous_module_id,
                previous_function_id,
                module_id,
                function_id,
            } => write!(
                f,
                "AOT unit ({module_id}, {function_id}) follows ({previous_module_id}, {previous_function_id}) out of order"
            ),
            AbiError::DuplicateFunction {
                module_id,
                function_id,
            } => write!(
                f,
                "AOT image has duplicate native function ({module_id}, {function_id})"
            ),
            AbiError::EntryFunctionMissing {
                module_id,
                function_id,
            } => write!(
                f,
                "AOT image entry function ({module_id}, {function_id}) is absent"
            ),
            AbiError::UnknownFunction {
                module_id,
                function_id,
            } => write!(
                f,
                "no native entry for function ({module_id}, {function_id})"
            ),
        }
    }
}

impl std::error::Error for AbiError {}

/// Reads and validates the generated AOT image descriptor.
///
/// Feature-gated (`aot-image`) because it references the external
/// # Safety
///
/// The caller must provide a live, uniquely owned `frame` whose nonempty handle
/// range is disjoint from its header, and a live, aligned, writable `out` when
/// this helper has one. Both remain valid and unaliased for the full call.
///
/// `bamts_program_descriptor` symbol, which only exists in a fully linked AOT
/// binary. The descriptor types and [`LinkedProgram::from_descriptor`] validator
/// are always available for synthetic use and testing.
#[cfg(feature = "aot-image")]
pub fn linked_program() -> Result<LinkedProgram<'static>, AbiError> {
    unsafe extern "C" {
        static bamts_program_descriptor: ProgramDescriptor;
    }
    // SAFETY: `bamts_program_descriptor` is the generated 'static AOT image; its
    // pointer fields address 'static, immutable bytecode and unit tables, so
    // reading it and borrowing for 'static is sound.
    unsafe { LinkedProgram::from_descriptor(&bamts_program_descriptor) }
}

// -- JIT entry (feature `jit-entry`) -----------------------------------------

#[cfg(feature = "jit-entry")]
pub use jit::JitEntry;

#[cfg(feature = "jit-entry")]
mod jit {
    use core::marker::PhantomData;

    use cranelift_jit::JITModule;
    use cranelift_module::FuncId;

    use super::{Completion, CompletionTag, NativeEntryFn, ShadowFrame, call_native_entry};

    /// A finalized JIT entry point, bound to its owning `JITModule`'s lifetime so
    /// the code cannot be invoked after the module frees its memory.
    pub struct JitEntry<'m> {
        entry: NativeEntryFn,
        _module: PhantomData<&'m JITModule>,
    }

    impl<'m> JitEntry<'m> {
        /// Resolves the finalized entry for `func` in `module`.
        ///
        /// `func` must have been defined and finalized in `module` with the
        /// native-entry signature `(frame, out) -> tag` (every lowered function
        /// does). The returned entry borrows `module` for `'m`, so it cannot
        /// outlive the code it points at.
        #[must_use]
        pub fn new(module: &'m JITModule, func: FuncId) -> JitEntry<'m> {
            let ptr = module.get_finalized_function(func);
            // SAFETY: `ptr` is the finalized machine code for `func` — initialized
            // and non-null — living in the module's executable mapping
            // (provenance). Every lowered function is emitted with the
            // native-entry ABI `extern "C" fn(*mut ShadowFrame, *mut Completion)
            // -> u32`, so the thin code pointer and `NativeEntryFn` share size and
            // alignment and the reinterpretation is valid (no bounds involved).
            // The code stays mapped for `'m` because `JitEntry` borrows `module`.
            let entry: NativeEntryFn =
                unsafe { core::mem::transmute::<*const u8, NativeEntryFn>(ptr) };
            JitEntry {
                entry,
                _module: PhantomData,
            }
        }

        /// The raw finalized entry pointer.
        #[inline]
        #[must_use]
        pub fn entry_fn(&self) -> NativeEntryFn {
            self.entry
        }

        /// Invokes the entry, returning its completion tag.
        pub fn invoke(&self, frame: &mut ShadowFrame, out: &mut Completion) -> CompletionTag {
            // SAFETY: `entry` is a finalized native entry with the native-entry
            // ABI, and its module (`'m`) outlives `self`, so the code is mapped.
            unsafe { call_native_entry(self.entry, frame, out) }
        }
    }
}

#[cfg(test)]
mod tests {
    fn test_bamts_load_this(f: *mut ShadowFrame, o: *mut Completion) -> u32 {
        unsafe { super::bamts_load_this(f, o) }
    }
    fn test_bamts_call(f: *mut ShadowFrame, a: u64, t: u64, x: u64, o: *mut Completion) -> u32 {
        unsafe { super::bamts_call(f, a, t, x, o) }
    }
    fn test_bamts_binary(f: *mut ShadowFrame, op: u32, l: u64, r: u64, o: *mut Completion) -> u32 {
        unsafe { super::bamts_binary(f, op, l, r, o) }
    }
    fn test_bamts_truthy(f: *mut ShadowFrame, v: u64) -> u32 {
        unsafe { super::bamts_truthy(f, v) }
    }
    fn test_bamts_iterator_next(
        f: *mut ShadowFrame,
        i: u64,
        d: u32,
        v: u32,
        o: *mut Completion,
    ) -> u32 {
        unsafe { super::bamts_iterator_next(f, i, d, v, o) }
    }
    fn test_bamts_create_object(f: *mut ShadowFrame, o: *mut Completion) -> u32 {
        unsafe { super::bamts_create_object(f, o) }
    }
    fn test_bamts_load_global(f: *mut ShadowFrame, n: u32, o: *mut Completion) -> u32 {
        unsafe { super::bamts_load_global(f, n, o) }
    }

    use super::*;
    use crate::{Completion, CompletionTag, ShadowFrame, Value};
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// The codegen helper table, restated here as the parity fixture. A direct
    /// comparison to `bamts_codegen::Helper` is impossible (codegen depends on
    /// this crate under `host-jit`, so importing it would be a cycle), so this
    /// literal is the pinned contract; it must stay byte-identical to
    /// # Safety
    ///
    /// The caller must provide a live, uniquely owned `frame` whose nonempty handle
    /// range is disjoint from its header, and a live, aligned, writable `out` when
    /// this helper has one. Both remain valid and unaliased for the full call.
    ///
    /// `bamts_codegen::Helper::{external_index, symbol}`.
    const CODEGEN_HELPERS: [(u32, &str); 30] = [
        (0, "bamts_load_constant"),
        (1, "bamts_unary"),
        (2, "bamts_binary"),
        (3, "bamts_create_object"),
        (4, "bamts_create_array"),
        (5, "bamts_create_closure"),
        (6, "bamts_get_property"),
        (7, "bamts_set_property"),
        (8, "bamts_delete_property"),
        (9, "bamts_call"),
        (10, "bamts_construct"),
        (11, "bamts_import"),
        (12, "bamts_truthy"),
        (13, "bamts_resume_value"),
        (14, "bamts_define_accessor"),
        (15, "bamts_load_global"),
        (16, "bamts_store_global"),
        (17, "bamts_typeof_global"),
        (18, "bamts_load_this"),
        (19, "bamts_load_arguments"),
        (20, "bamts_load_new_target"),
        (21, "bamts_array_push"),
        (22, "bamts_array_extend"),
        (23, "bamts_object_spread"),
        (24, "bamts_set_prototype"),
        (25, "bamts_create_private_name"),
        (26, "bamts_create_regexp"),
        (27, "bamts_get_iterator"),
        (28, "bamts_iterator_next"),
        (29, "bamts_export"),
    ];

    /// A recording dispatcher: captures the last call and returns a fixed
    /// result. Uses interior mutability since [`NativeOps`] dispatches on `&self`.
    struct Recorder {
        last: Cell<Option<HelperCall>>,
        truthy_calls: Cell<u32>,
        truthy_answer: Cell<bool>,
        result: HelperResult,
    }

    impl Recorder {
        fn normal(value: Value) -> Recorder {
            Recorder {
                last: Cell::new(None),
                truthy_calls: Cell::new(0),
                truthy_answer: Cell::new(false),
                result: HelperResult::normal(value),
            }
        }
    }

    impl NativeOps for Recorder {
        fn truthy(&self, _frame: &mut NativeFrame<'_>, value: Value) -> bool {
            self.truthy_calls.set(self.truthy_calls.get() + 1);
            self.last.set(Some(HelperCall::Truthy { value }));
            self.truthy_answer.get()
        }

        fn dispatch(&self, frame: &mut NativeFrame<'_>, call: HelperCall) -> HelperResult {
            self.last.set(Some(call));
            if let HelperCall::IteratorNext {
                done_reg,
                value_reg,
                ..
            } = call
            {
                frame.set_register(done_reg, Value::TRUE);
                frame.set_register(value_reg, Value::int32(9));
            }
            self.result
        }
    }

    /// A dispatcher whose `dispatch` always panics, to exercise the boundary.
    struct Panicky;

    impl NativeOps for Panicky {
        fn truthy(&self, _frame: &mut NativeFrame<'_>, _value: Value) -> bool {
            panic!("truthy panic")
        }

        fn dispatch(&self, _frame: &mut NativeFrame<'_>, _call: HelperCall) -> HelperResult {
            panic!("dispatch panic")
        }
    }

    /// Runs `f`, catching an expected panic at the boundary. Expected panics
    /// still print one line each; that is normal test behavior and avoids the
    /// flakiness of swapping the process-global panic hook under parallel tests.
    fn quietly<R>(f: impl FnOnce() -> R + std::panic::UnwindSafe) -> std::thread::Result<R> {
        catch_unwind(f)
    }

    fn frame_with(regs: &mut [Value]) -> ShadowFrame {
        let len = u16::try_from(regs.len()).expect("register count fits u16");
        ShadowFrame::new(core::ptr::null_mut(), 0, 0, regs.as_mut_ptr(), len)
    }

    /// A dispatcher that re-enters itself once on the outer `Call`: it fires a
    /// nested `bamts_load_this` on a distinct child frame through the same TLS
    /// `&self`, then mutates its own state after the nested call returns. This is
    /// the exact single-instance reentry pattern that `&mut self` could not
    /// support soundly. State is behind `Cell` (interior mutability).
    struct Reentrant {
        depth: Cell<u32>,
        max_depth: Cell<u32>,
        post_nested_ran: Cell<bool>,
    }

    impl NativeOps for Reentrant {
        fn truthy(&self, _frame: &mut NativeFrame<'_>, _value: Value) -> bool {
            true
        }

        fn dispatch(&self, _frame: &mut NativeFrame<'_>, call: HelperCall) -> HelperResult {
            let depth = self.depth.get();
            self.max_depth.set(self.max_depth.get().max(depth));
            if depth == 0 && matches!(call, HelperCall::Call { .. }) {
                self.depth.set(1);
                // Re-enter native code on a DISTINCT child frame; this dispatches
                // back into the same `&self` via the TLS seam.
                let mut child_regs = [Value::UNINITIALIZED; 1];
                let mut child_frame = frame_with(&mut child_regs);
                let mut child_out = Completion::new(Value::UNDEFINED);
                let nested = test_bamts_load_this(&mut child_frame, &mut child_out);
                assert_eq!(nested, CompletionTag::Normal.as_u32());
                self.depth.set(0);
                // Post-nested-call state mutation — UB under `&mut self`, sound here.
                self.post_nested_ran.set(true);
            }
            HelperResult::normal(Value::int32(depth as i32 as u32))
        }
    }

    #[test]
    fn same_instance_reentry_mutates_state_after_nested_call() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Reentrant {
            depth: Cell::new(0),
            max_depth: Cell::new(0),
            post_nested_ran: Cell::new(false),
        };
        let tag = with_native_ops(&mut ops, || {
            test_bamts_call(
                &mut frame,
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                &mut completion,
            )
        });
        assert_eq!(tag, CompletionTag::Normal.as_u32());
        // The nested dispatch re-entered the same instance (depth reached 1).
        assert_eq!(ops.max_depth.get(), 1);
        // And the outer dispatch resumed touching `self` after the nested return.
        assert!(ops.post_nested_ran.get());
    }

    #[test]
    fn helper_symbols_and_indices_match_codegen() {
        assert_eq!(HELPER_COUNT as usize, CODEGEN_HELPERS.len());
        for (index, symbol) in CODEGEN_HELPERS {
            let helper = NativeHelper::from_u32(index).expect("dense index");
            assert_eq!(helper.as_u32(), index, "index for {helper:?}");
            assert_eq!(helper.symbol(), symbol, "symbol for {helper:?}");
        }
        // Dense and total: no index past the table, and the inverse rejects it.
        assert_eq!(NativeHelper::from_u32(HELPER_COUNT), None);
    }

    #[test]
    fn helper_call_maps_to_its_helper() {
        assert_eq!(
            HelperCall::Binary {
                op: 0,
                left: Value::UNDEFINED,
                right: Value::UNDEFINED,
            }
            .helper(),
            NativeHelper::Binary
        );
        assert_eq!(HelperCall::ResumeValue.helper(), NativeHelper::ResumeValue);
        assert_eq!(
            HelperCall::Truthy { value: Value::TRUE }.helper(),
            NativeHelper::Truthy
        );
        assert_eq!(
            HelperCall::Export {
                name: 3,
                src: Value::NULL,
            }
            .helper(),
            NativeHelper::Export
        );
    }

    #[test]
    fn exported_wrapper_dispatches_and_writes_completion() {
        let mut regs = [Value::UNINITIALIZED; 2];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Recorder::normal(Value::int32(42));
        let tag = with_native_ops(&mut ops, || {
            test_bamts_binary(
                &mut frame,
                2,
                Value::int32(3).to_bits(),
                Value::int32(4).to_bits(),
                &mut completion,
            )
        });
        assert_eq!(tag, CompletionTag::Normal.as_u32());
        assert_eq!(completion.value.as_int32(), Some(42));
        assert_eq!(
            ops.last.get(),
            Some(HelperCall::Binary {
                op: 2,
                left: Value::int32(3),
                right: Value::int32(4),
            })
        );
    }

    #[test]
    fn truthy_wrapper_routes_to_truthy_not_dispatch() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut ops = Recorder::normal(Value::UNDEFINED);
        ops.truthy_answer.set(true);
        let truthy = with_native_ops(&mut ops, || {
            test_bamts_truthy(&mut frame, Value::int32(1).to_bits())
        });
        assert_eq!(truthy, 1);
        assert_eq!(ops.truthy_calls.get(), 1);

        ops.truthy_answer.set(false);
        let falsy = with_native_ops(&mut ops, || {
            test_bamts_truthy(&mut frame, Value::int32(0).to_bits())
        });
        assert_eq!(falsy, 0);
        assert_eq!(ops.truthy_calls.get(), 2);
    }

    #[test]
    fn iterator_next_writes_both_registers() {
        let mut regs = [Value::UNINITIALIZED; 2];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Recorder::normal(Value::UNDEFINED);
        let tag = with_native_ops(&mut ops, || {
            test_bamts_iterator_next(&mut frame, Value::NULL.to_bits(), 0, 1, &mut completion)
        });
        assert_eq!(tag, CompletionTag::Normal.as_u32());
        assert_eq!(regs[0], Value::TRUE);
        assert_eq!(regs[1], Value::int32(9));
    }

    #[test]
    fn missing_dispatcher_is_a_fatal_trap() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        // No `with_native_ops` scope: no dispatcher installed.
        let tag = test_bamts_create_object(&mut frame, &mut completion);
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
        assert_eq!(completion.value.as_int32(), Some(TRAP_MISSING_NATIVE_OPS));
        // The tagless helper has only `0` to report the failure with.
        assert_eq!(test_bamts_truthy(&mut frame, Value::TRUE.to_bits()), 0);
    }

    #[test]
    fn invalid_frame_is_a_fatal_trap() {
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Recorder::normal(Value::int32(1));
        let null = with_native_ops(&mut ops, || {
            test_bamts_create_object(core::ptr::null_mut(), &mut completion)
        });
        assert_eq!(null, CompletionTag::FatalTrap.as_u32());
        assert_eq!(completion.value.as_int32(), Some(TRAP_INVALID_FRAME));
        // A misaligned (non-null) pointer is rejected without a dereference.
        let misaligned = with_native_ops(&mut ops, || {
            test_bamts_create_object(
                core::ptr::null_mut::<ShadowFrame>().wrapping_byte_add(1),
                &mut completion,
            )
        });
        assert_eq!(misaligned, CompletionTag::FatalTrap.as_u32());
    }

    #[test]
    fn dispatcher_panic_is_caught_as_fatal_trap() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Panicky;
        let tag = quietly(AssertUnwindSafe(|| {
            with_native_ops(&mut ops, || {
                test_bamts_create_object(&mut frame, &mut completion)
            })
        }))
        .expect("wrapper must not unwind across the boundary");
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
        assert_eq!(completion.value.as_int32(), Some(TRAP_PANIC));
    }

    #[test]
    fn tls_nesting_restores_the_outer_dispatcher() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut outer = Recorder::normal(Value::int32(1));
        let mut inner = Recorder::normal(Value::int32(2));

        with_native_ops(&mut outer, || {
            test_bamts_create_object(&mut frame, &mut completion);
            assert_eq!(completion.value.as_int32(), Some(1));
            with_native_ops(&mut inner, || {
                test_bamts_create_object(&mut frame, &mut completion);
                assert_eq!(completion.value.as_int32(), Some(2));
            });
            // The inner scope has ended: the outer dispatcher is restored.
            test_bamts_create_object(&mut frame, &mut completion);
            assert_eq!(completion.value.as_int32(), Some(1));
        });
        // Both scopes ended: no dispatcher, so a fatal trap.
        let tag = test_bamts_create_object(&mut frame, &mut completion);
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
    }

    #[test]
    fn tls_is_restored_after_a_panicking_body() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Recorder::normal(Value::int32(1));

        let result = quietly(AssertUnwindSafe(|| {
            with_native_ops(&mut ops, || panic!("body panic"));
        }));
        assert!(result.is_err());
        // The guard restored the previous (empty) dispatcher on unwind.
        let tag = test_bamts_create_object(&mut frame, &mut completion);
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
    }

    #[test]
    fn native_frame_new_validates_metadata() {
        let mut regs = [Value::int32(1), Value::int32(2)];
        let base = regs.as_mut_ptr();
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, 3, base, 2);
        // Length mismatch against the frame header.
        {
            let mut short = [Value::int32(1)];
            assert!(NativeFrame::new(&mut frame, &mut short).is_none());
        }
        // Pointer mismatch against the frame header.
        {
            let mut other = [Value::int32(1), Value::int32(2)];
            assert!(NativeFrame::new(&mut frame, &mut other).is_none());
        }
        // Exact match.
        {
            let native = NativeFrame::new(&mut frame, &mut regs);
            assert!(native.is_some());
        }
    }

    #[test]
    fn native_frame_from_raw_validates_and_addresses_registers() {
        // SAFETY: from_raw rejects a null or misaligned pointer before any
        // dereference, so neither call accesses memory.
        assert!(unsafe { NativeFrame::from_raw(core::ptr::null_mut()) }.is_none());
        assert!(
            unsafe {
                NativeFrame::from_raw(core::ptr::null_mut::<ShadowFrame>().wrapping_byte_add(1))
            }
            .is_none()
        );

        let mut regs = [Value::int32(10), Value::int32(20)];
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 7, 11, regs.as_mut_ptr(), 2);
        {
            // SAFETY: `frame` is a live, unaliased local and its metadata points
            // at exactly the two initialized Values in `regs` for this scope.
            let mut native = unsafe { NativeFrame::from_raw(&mut frame) }.expect("valid frame");
            assert_eq!(native.handle_len(), 2);
            assert_eq!(native.module_id(), 11);
            assert_eq!(native.pc(), 7);
            assert_eq!(native.register(0), Value::int32(10));
            assert_eq!(native.try_register(5), None);
            native.set_register(1, Value::int32(99));
            assert!(native.try_set_register(1, Value::int32(99)));
            assert!(!native.try_set_register(5, Value::int32(0)));
            native.set_resume(3);
        }
        assert_eq!(frame.bytecode_pc, 3);
        assert_eq!(regs[1], Value::int32(99));
    }

    #[test]
    fn native_frame_from_raw_rejects_handles_overlapping_header() {
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, 0, core::ptr::null_mut(), 1);
        frame.handles = core::ptr::addr_of_mut!(frame).cast::<Value>();
        // SAFETY: `frame` is live and aligned; this test intentionally supplies
        // malformed metadata to verify it is rejected before aliasing references.
        assert!(unsafe { NativeFrame::from_raw(&mut frame) }.is_none());
    }

    unsafe extern "C" fn entry_returns_seven(
        _frame: *mut ShadowFrame,
        out: *mut Completion,
    ) -> u32 {
        // SAFETY: the test passes a valid, writable `Completion` out-parameter.
        unsafe { core::ptr::write(out, Completion::new(Value::int32(7))) };
        CompletionTag::Normal.as_u32()
    }

    unsafe extern "C" fn entry_returns_invalid_tag(
        _frame: *mut ShadowFrame,
        out: *mut Completion,
    ) -> u32 {
        // SAFETY: the test passes a valid, writable completion pointer.
        unsafe { core::ptr::write(out, Completion::new(Value::int32(99))) };
        u32::MAX
    }

    #[test]
    fn invalid_native_completion_tag_replaces_stale_output() {
        let mut regs: [Value; 0] = [];
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, 0, regs.as_mut_ptr(), 0);
        let mut out = Completion::new(Value::int32(123));
        // SAFETY: test entry has the native ABI and frame/output are live, unique.
        let tag = unsafe { call_native_entry(entry_returns_invalid_tag, &mut frame, &mut out) };
        assert_eq!(tag, CompletionTag::FatalTrap);
        assert_eq!(out.value.as_int32(), Some(TRAP_INVALID_COMPLETION_TAG));
    }

    fn unit(module_id: u32, function_id: u32) -> UnitDescriptor {
        UnitDescriptor {
            function_id,
            module_id,
            entry: entry_returns_seven,
        }
    }

    fn program(
        bytecode: &[u8],
        units: &[UnitDescriptor],
        entry_module: u32,
        entry_function: u32,
    ) -> ProgramDescriptor {
        ProgramDescriptor {
            magic: AOT_MAGIC,
            abi_version: AOT_ABI_VERSION,
            flags: 0,
            bytecode: bytecode.as_ptr(),
            bytecode_len: bytecode.len(),
            units: units.as_ptr(),
            unit_count: units.len(),
            entry_function,
            entry_module,
        }
    }

    /// Validates a locally-built descriptor after proving its raw pointer fields
    /// describe the supplied backing slices.
    fn linked_of<'a>(
        descriptor: &'a ProgramDescriptor,
        bytecode: &'a [u8],
        units: &'a [UnitDescriptor],
    ) -> Result<LinkedProgram<'a>, AbiError> {
        assert_eq!(descriptor.bytecode_len, bytecode.len());
        assert!(bytecode.is_empty() || core::ptr::eq(descriptor.bytecode, bytecode.as_ptr()));
        assert_eq!(descriptor.unit_count, units.len());
        assert!(units.is_empty() || core::ptr::eq(descriptor.units, units.as_ptr()));
        // SAFETY: the pointer/length pairs were checked against live immutable
        // backing slices above; both slices outlive the returned program view.
        unsafe { LinkedProgram::from_descriptor(descriptor) }
    }

    #[test]
    fn linked_program_validates_and_invokes_tuple_identities() {
        let bytecode = [1u8, 2, 3];
        let units = [unit(2, 4), unit(2, 5), unit(3, 5)];
        let descriptor = program(&bytecode, &units, 3, 5);
        let linked = linked_of(&descriptor, &bytecode, &units).expect("valid image");
        assert_eq!(linked.bytecode(), &[1, 2, 3]);
        assert_eq!(linked.program_bytes(), &[1, 2, 3]);
        assert_eq!(linked.units().len(), 3);
        assert_eq!(linked.entry_module(), 3);
        assert_eq!(linked.entry_function(), 5);
        assert!(linked.unit(2, 5).is_some());
        assert!(linked.unit(3, 5).is_some());
        assert!(linked.unit(3, 4).is_none());

        let mut regs: [Value; 0] = [];
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, 3, regs.as_mut_ptr(), 0);
        let mut completion = Completion::new(Value::UNDEFINED);
        let tag = linked
            .invoke(3, 5, &mut frame, &mut completion)
            .expect("entry present");
        assert_eq!(tag, CompletionTag::Normal);
        assert_eq!(completion.value.as_int32(), Some(7));
        assert_eq!(
            linked.invoke(4, 5, &mut frame, &mut completion).err(),
            Some(AbiError::UnknownFunction {
                module_id: 4,
                function_id: 5,
            })
        );
    }

    #[test]
    fn linked_program_rejects_malformed_descriptors() {
        let bytecode = [1u8, 2, 3];
        let units = [unit(2, 5)];

        let mut bad_magic = program(&bytecode, &units, 2, 5);
        bad_magic.magic = 0;
        assert_eq!(
            linked_of(&bad_magic, &bytecode, &units).err(),
            Some(AbiError::BadMagic { found: 0 })
        );

        let mut bad_version = program(&bytecode, &units, 2, 5);
        bad_version.abi_version = 1;
        assert_eq!(
            linked_of(&bad_version, &bytecode, &units).err(),
            Some(AbiError::UnsupportedAbiVersion { found: 1 })
        );

        let mut bad_flags = program(&bytecode, &units, 2, 5);
        bad_flags.flags = 1;
        assert_eq!(
            linked_of(&bad_flags, &bytecode, &units).err(),
            Some(AbiError::NonZeroFlags { flags: 1 })
        );

        let empty_bytecode = program(&[], &units, 2, 5);
        assert_eq!(
            linked_of(&empty_bytecode, &[], &units).err(),
            Some(AbiError::EmptyBytecode)
        );

        let empty_units = program(&bytecode, &[], 2, 5);
        assert_eq!(
            linked_of(&empty_units, &bytecode, &[]).err(),
            Some(AbiError::EmptyUnits)
        );
    }

    #[test]
    fn linked_program_rejects_unsorted_duplicate_and_missing_tuple_entries() {
        let bytecode = [1u8, 2, 3];

        let unsorted = [unit(2, 5), unit(1, 9)];
        let unsorted_descriptor = program(&bytecode, &unsorted, 2, 5);
        assert_eq!(
            linked_of(&unsorted_descriptor, &bytecode, &unsorted).err(),
            Some(AbiError::UnsortedUnits {
                previous_module_id: 2,
                previous_function_id: 5,
                module_id: 1,
                function_id: 9,
            })
        );

        let duplicate = [unit(2, 5), unit(2, 5)];
        let duplicate_descriptor = program(&bytecode, &duplicate, 2, 5);
        assert_eq!(
            linked_of(&duplicate_descriptor, &bytecode, &duplicate).err(),
            Some(AbiError::DuplicateFunction {
                module_id: 2,
                function_id: 5,
            })
        );

        let units = [unit(2, 5), unit(3, 5)];
        let missing_entry = program(&bytecode, &units, 4, 5);
        assert_eq!(
            linked_of(&missing_entry, &bytecode, &units).err(),
            Some(AbiError::EntryFunctionMissing {
                module_id: 4,
                function_id: 5,
            })
        );
    }

    #[test]
    fn null_out_is_a_fatal_trap_without_dereference() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut ops = Recorder::normal(Value::int32(1));
        // Null `out`: the wrapper returns the fatal tag and never writes a body.
        let tag = with_native_ops(&mut ops, || {
            test_bamts_load_global(&mut frame, 0, core::ptr::null_mut())
        });
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
    }

    #[test]
    fn iterator_next_out_of_range_register_is_fatal_trap() {
        let mut regs = [Value::UNINITIALIZED; 1];
        let mut frame = frame_with(&mut regs);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = Recorder::normal(Value::UNDEFINED);
        let tag = with_native_ops(&mut ops, || {
            test_bamts_iterator_next(&mut frame, Value::NULL.to_bits(), 99, 0, &mut completion)
        });
        assert_eq!(tag, CompletionTag::FatalTrap.as_u32());
        assert_eq!(completion.value.as_int32(), Some(TRAP_INVALID_REGISTER));
        // The dispatcher was never reached, so no register was mutated.
        assert_eq!(regs[0], Value::UNINITIALIZED);
    }
}
