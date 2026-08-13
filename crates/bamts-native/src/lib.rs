//! Native ABI foundations for BamTiScript.
//!
//! This module owns the C-layout value representation shared verbatim between
//! the register interpreter and generated native code. `Value` constants and
//! `ShadowFrame` layout are grounded in the machine-checked formal models:
//!
//! * `formal/lean/Bamti/Value.lean` — the NaN-boxed `Word64` field algebra
//!   (`header:13 || tag:3 || payload:48`), the seven nonzero tags, and the
//!   `encode`/`decode` round-trip theorem.
//! * `formal/lean/Bamti/Abi.lean` — the 32-byte, 8-aligned `ShadowFrame`
//!   header layout theorem.
//!
//! `Completion` and `CompletionTag` belong to the native-entry contract in the
//! canonical execution plan (N5); the Lean files do not assign them a wire
//! layout. `Bamti.NodeLoop.Completion` is a separate event-loop proof record.
//!
//! The value/frame primitives in this file are total, allocation-free, and
//! require no `unsafe`. The native runtime bridge — the exported `bamts_*`
//! helper ABI, the panic- and nesting-safe [`native_bridge::NativeOps`]
//! dispatch seam, and the feature-gated JIT/AOT linkage surfaces — lives in
//! [`native_bridge`], which centralizes every `unsafe` operation the generated
//! code requires. The feature-gated Windows cache capability lives in
//! [`cache_guard`], whose Win32 implementation isolates its `unsafe` operations
//! in `cache_guard/windows.rs`.

use core::num::{NonZeroU16, NonZeroU32};

// -- NaN-box field constants (grounded in Value.lean) ------------------------

/// Header bits 63..51. `canonicalHeader = 4095`, so `4095 << 51`.
const HEADER_SHIFT: u32 = 51;

/// Tag bits 50..48, immediately below the 13-bit header.
const TAG_SHIFT: u32 = 48;

/// The three-bit tag field mask.
const TAG_MASK: u64 = 0b111;

/// The low 48 payload bits (`payloadToNat` domain).
const PAYLOAD_MASK: u64 = (1u64 << 48) - 1;

/// The `upper : u16` half of the payload, bits 47..32 (`packSlot` segment).
const UPPER_MASK: u64 = 0xFFFF_0000_0000;

/// The full 13-bit header field mask, bits 63..51.
const HEADER_MASK: u64 = 0x1FFFu64 << HEADER_SHIFT;

/// The canonical positive quiet NaN, `0x7ff8_0000_0000_0000`.
const CANON_NAN: u64 = 4095u64 << HEADER_SHIFT;

/// Tag code for a heap reference (`SlotId`).
pub const TAG_HEAP_REF: u8 = 1;
/// Tag code for a boxed 32-bit integer.
pub const TAG_INT32: u8 = 2;
/// Tag code for `undefined`.
pub const TAG_UNDEFINED: u8 = 3;
/// Tag code for `null`.
pub const TAG_NULL: u8 = 4;
/// Tag code for a boolean.
pub const TAG_BOOLEAN: u8 = 5;
/// Tag code for the array/TDZ hole.
pub const TAG_HOLE: u8 = 6;
/// Tag code for an uninitialized register slot.
pub const TAG_UNINITIALIZED: u8 = 7;

#[inline]
const fn tag_of(bits: u64) -> u64 {
    (bits >> TAG_SHIFT) & TAG_MASK
}

/// A word is boxed when it carries the canonical NaN header and a nonzero tag
/// (`isBoxedWire`). Every non-boxed word is an arithmetic double.
#[inline]
const fn is_boxed(bits: u64) -> bool {
    (bits & HEADER_MASK) == CANON_NAN && tag_of(bits) != 0
}

/// `CANON_NAN | (tag << 48) | payload`, matching `boxedWord`.
///
/// `tag` must be one of the seven codes (`< 8`) and `payload` must fit in 48
/// bits; both hold for every internal caller.
#[inline]
const fn boxed(tag: u8, payload: u64) -> u64 {
    CANON_NAN | ((tag as u64) << TAG_SHIFT) | payload
}

// -- Heap slot identity (grounded in Value.lean `SlotId`) --------------------

/// A validated heap-reference payload: `segment:u16 << 32 | slot:u32`, both
/// nonzero. Illegal (zero) identities are unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SlotId {
    segment: NonZeroU16,
    slot: NonZeroU32,
}

impl SlotId {
    /// The total constructor over already-validated nonzero fields.
    #[inline]
    pub const fn new(segment: NonZeroU16, slot: NonZeroU32) -> Self {
        Self { segment, slot }
    }

    /// Parses raw parts, rejecting a zero segment or zero slot (`unpackSlot`).
    #[inline]
    pub const fn from_parts(segment: u16, slot: u32) -> Option<Self> {
        match (NonZeroU16::new(segment), NonZeroU32::new(slot)) {
            (Some(segment), Some(slot)) => Some(Self { segment, slot }),
            _ => None,
        }
    }

    /// The nonzero segment half.
    #[inline]
    pub const fn segment(self) -> u16 {
        self.segment.get()
    }

    /// The nonzero slot half.
    #[inline]
    pub const fn slot(self) -> u32 {
        self.slot.get()
    }

    /// The 48-bit packed payload, `packSlot`.
    #[inline]
    const fn payload(self) -> u64 {
        ((self.segment.get() as u64) << 32) | self.slot.get() as u64
    }
}

// -- Value (grounded in Value.lean `Value`/`encode`/`decode`) ----------------

/// A NaN-boxed JavaScript value. ABI-identical to a `u64`, so `*mut Value`
/// arrays and `Completion` fields carry it with no wrapping.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64);

/// The decoded meaning of a `Value`, mirroring the `Value` inductive.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decoded {
    /// A non-boxed IEEE-754 double.
    Number(f64),
    /// A validated heap reference.
    HeapRef(SlotId),
    /// A boxed 32-bit integer.
    Int32(u32),
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A boolean.
    Boolean(bool),
    /// The array/TDZ hole.
    Hole,
    /// An uninitialized register slot.
    Uninitialized,
}

impl Value {
    /// The canonical positive quiet NaN, `0x7ff8_0000_0000_0000`.
    pub const CANON_NAN: u64 = CANON_NAN;

    /// `undefined`.
    pub const UNDEFINED: Value = Value(boxed(TAG_UNDEFINED, 0));
    /// `null`.
    pub const NULL: Value = Value(boxed(TAG_NULL, 0));
    /// The array/TDZ hole.
    pub const HOLE: Value = Value(boxed(TAG_HOLE, 0));
    /// The uninitialized register-slot sentinel written by the frame prologue.
    pub const UNINITIALIZED: Value = Value(boxed(TAG_UNINITIALIZED, 0));
    /// `false`.
    pub const FALSE: Value = Value(boxed(TAG_BOOLEAN, 0));
    /// `true`.
    pub const TRUE: Value = Value(boxed(TAG_BOOLEAN, 1));

    /// Boxes a boolean (`boolPayload`).
    #[inline]
    pub const fn boolean(value: bool) -> Value {
        Value(boxed(TAG_BOOLEAN, value as u64))
    }

    /// Boxes a 32-bit integer (`int32Payload`, upper half zero).
    #[inline]
    pub const fn int32(value: u32) -> Value {
        Value(boxed(TAG_INT32, value as u64))
    }

    /// Boxes a validated heap reference (`packSlot`).
    #[inline]
    pub const fn heap_ref(id: SlotId) -> Value {
        Value(boxed(TAG_HEAP_REF, id.payload()))
    }

    /// Encodes a double. Every NaN is canonicalized to `CANON_NAN`, so the
    /// result never collides with the boxed range (`canonicalizeNaN`).
    #[inline]
    pub fn number(value: f64) -> Value {
        if value.is_nan() {
            Value(CANON_NAN)
        } else {
            Value(value.to_bits())
        }
    }

    /// Reinterprets a raw 64-bit ABI word as a `Value`. The wire word is taken
    /// verbatim; use [`Value::decode`] to interpret it.
    #[inline]
    pub const fn from_bits(bits: u64) -> Value {
        Value(bits)
    }

    /// The raw 64-bit ABI word.
    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Whether the word is a non-boxed arithmetic double.
    #[inline]
    pub const fn is_number(self) -> bool {
        !is_boxed(self.0)
    }

    /// Whether the word is exactly the uninitialized sentinel. This is the
    /// hot check the frame prologue and GC scan rely on.
    #[inline]
    pub const fn is_uninitialized(self) -> bool {
        self.0 == Value::UNINITIALIZED.0
    }

    /// Interprets the word per the tag-specific payload rules (`decode`).
    /// Returns `None` for a boxed word whose payload is malformed for its tag.
    pub const fn decode(self) -> Option<Decoded> {
        let bits = self.0;
        if !is_boxed(bits) {
            return Some(Decoded::Number(f64::from_bits(bits)));
        }
        let payload = bits & PAYLOAD_MASK;
        let upper = (payload >> 32) as u16;
        let lower = payload as u32;
        match tag_of(bits) as u8 {
            TAG_HEAP_REF => match SlotId::from_parts(upper, lower) {
                Some(id) => Some(Decoded::HeapRef(id)),
                None => None,
            },
            TAG_INT32 => {
                if upper == 0 {
                    Some(Decoded::Int32(lower))
                } else {
                    None
                }
            }
            TAG_UNDEFINED => {
                if payload == 0 {
                    Some(Decoded::Undefined)
                } else {
                    None
                }
            }
            TAG_NULL => {
                if payload == 0 {
                    Some(Decoded::Null)
                } else {
                    None
                }
            }
            TAG_BOOLEAN => match payload {
                0 => Some(Decoded::Boolean(false)),
                1 => Some(Decoded::Boolean(true)),
                _ => None,
            },
            TAG_HOLE => {
                if payload == 0 {
                    Some(Decoded::Hole)
                } else {
                    None
                }
            }
            TAG_UNINITIALIZED => {
                if payload == 0 {
                    Some(Decoded::Uninitialized)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The double value, when the word is a non-boxed number.
    #[inline]
    pub fn as_f64(self) -> Option<f64> {
        match self.decode() {
            Some(Decoded::Number(value)) => Some(value),
            _ => None,
        }
    }

    /// The integer, when the word is a well-formed boxed `int32`.
    #[inline]
    pub const fn as_int32(self) -> Option<u32> {
        if is_boxed(self.0) && tag_of(self.0) as u8 == TAG_INT32 && (self.0 & UPPER_MASK) == 0 {
            Some(self.0 as u32)
        } else {
            None
        }
    }

    /// The boolean, when the word is a well-formed boxed boolean.
    #[inline]
    pub const fn as_bool(self) -> Option<bool> {
        if is_boxed(self.0) && tag_of(self.0) as u8 == TAG_BOOLEAN {
            match self.0 & PAYLOAD_MASK {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        } else {
            None
        }
    }

    /// The heap slot, when the word is a well-formed boxed reference.
    #[inline]
    pub const fn as_heap_ref(self) -> Option<SlotId> {
        if is_boxed(self.0) && tag_of(self.0) as u8 == TAG_HEAP_REF {
            let payload = self.0 & PAYLOAD_MASK;
            SlotId::from_parts((payload >> 32) as u16, payload as u32)
        } else {
            None
        }
    }
}

impl core::fmt::Debug for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.decode() {
            Some(decoded) => write!(f, "Value({decoded:?})"),
            None => write!(f, "Value(malformed {:#018x})", self.0),
        }
    }
}

// -- Entry ABI (grounded in the canonical plan N5) ---------------------------

/// The completion class returned by a native entry, as the raw `u32` result.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompletionTag {
    /// `out.value` is the return value.
    Normal = 0,
    /// `out.value` is a rooted error handle.
    Throw = 1,
    /// A resume offset was written to the frame; `out.value` is the yield.
    Suspend = 2,
    /// `out.value` encodes a `TrapRecordId`; control leaves to the runtime.
    FatalTrap = 3,
}

impl CompletionTag {
    /// The raw ABI discriminant.
    #[inline]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parses a raw ABI discriminant, rejecting values outside `0..=3`.
    #[inline]
    pub const fn from_u32(code: u32) -> Option<CompletionTag> {
        match code {
            0 => Some(CompletionTag::Normal),
            1 => Some(CompletionTag::Throw),
            2 => Some(CompletionTag::Suspend),
            3 => Some(CompletionTag::FatalTrap),
            _ => None,
        }
    }
}

/// The out-parameter written by a native entry. `size = 8`, `align = 8`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Completion {
    /// The completion value; its meaning is set by the returned [`CompletionTag`].
    pub value: Value,
}

impl Completion {
    /// Builds a completion carrying `value`.
    #[inline]
    pub const fn new(value: Value) -> Completion {
        Completion { value }
    }
}

// -- ShadowFrame (grounded in Abi.lean `ShadowFrame`/`shadowFrameBytes`) ------

/// The register frame header shared by the interpreter and native code.
///
/// Layout is fixed and identical on every 64-bit target: `previous` at 0,
/// `bytecode_pc` at 8, `module_id` at 12, `handles` at 16, and `handle_len` at
/// 24, with explicit zeroed trailing padding. `size = 32`, `align = 8`.
/// `handles` addresses exactly `handle_len` `Value`s.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShadowFrame {
    /// The caller's frame, or null at the base of the stack.
    pub previous: *mut ShadowFrame,
    /// The current bytecode program counter.
    pub bytecode_pc: u32,
    /// The dense module id of the executing function.
    pub module_id: u32,
    /// The register array; register `r[i]` is `handles[i]`.
    pub handles: *mut Value,
    /// The number of live registers, equal to `function.register_count`.
    pub handle_len: u16,
    _pad1: [u8; 6],
}

impl ShadowFrame {
    /// Builds a frame header with physically zeroed padding.
    #[inline]
    pub fn new(
        previous: *mut ShadowFrame,
        bytecode_pc: u32,
        module_id: u32,
        handles: *mut Value,
        handle_len: u16,
    ) -> ShadowFrame {
        ShadowFrame {
            previous,
            bytecode_pc,
            module_id,
            handles,
            handle_len,
            _pad1: [0; 6],
        }
    }
}

// -- Compile-time layout assertions ------------------------------------------

const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    // Value is ABI-identical to u64.
    assert!(size_of::<Value>() == 8);
    assert!(align_of::<Value>() == 8);

    // CANON_NAN equals the Lean canonicalHeader (4095) placed at bit 51.
    assert!(Value::CANON_NAN == 0x7ff8_0000_0000_0000);
    assert!(Value::CANON_NAN == 4095u64 << 51);

    // Completion: repr(C) { value: Value }, size 8, align 8, value at 0.
    assert!(size_of::<Completion>() == 8);
    assert!(align_of::<Completion>() == 8);
    assert!(offset_of!(Completion, value) == 0);

    // CompletionTag: repr(u32).
    assert!(size_of::<CompletionTag>() == 4);
    assert!(align_of::<CompletionTag>() == 4);

    // ShadowFrame: 32 bytes, 8-aligned, fields at 0/8/12/16/24 (Abi.lean).
    assert!(size_of::<ShadowFrame>() == 32);
    assert!(align_of::<ShadowFrame>() == 8);
    assert!(offset_of!(ShadowFrame, previous) == 0);
    assert!(offset_of!(ShadowFrame, bytecode_pc) == 8);
    assert!(offset_of!(ShadowFrame, module_id) == 12);
    assert!(offset_of!(ShadowFrame, handles) == 16);
    assert!(offset_of!(ShadowFrame, handle_len) == 24);
};

// -- Native runtime bridge ---------------------------------------------------

#[cfg(feature = "cache-guard")]
pub mod cache_guard;
pub mod native_bridge;
pub use native_bridge::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(segment: u16, slot: u32) -> SlotId {
        SlotId::from_parts(segment, slot).expect("nonzero parts")
    }

    #[test]
    fn canonical_singleton_bits_match_lean_layout() {
        // CANON_NAN | (tag << 48), grounded in tagCode and canonicalHeader.
        assert_eq!(Value::UNDEFINED.to_bits(), 0x7ffb_0000_0000_0000);
        assert_eq!(Value::NULL.to_bits(), 0x7ffc_0000_0000_0000);
        assert_eq!(Value::FALSE.to_bits(), 0x7ffd_0000_0000_0000);
        assert_eq!(Value::TRUE.to_bits(), 0x7ffd_0000_0000_0001);
        assert_eq!(Value::HOLE.to_bits(), 0x7ffe_0000_0000_0000);
        assert_eq!(Value::UNINITIALIZED.to_bits(), 0x7fff_0000_0000_0000);
        assert_eq!(Value::int32(0).to_bits(), 0x7ffa_0000_0000_0000);
        assert_eq!(Value::heap_ref(slot(1, 1)).to_bits(), 0x7ff9_0001_0000_0001);
    }

    #[test]
    fn decode_is_left_inverse_of_encode() {
        let cases = [
            (Value::UNDEFINED, Decoded::Undefined),
            (Value::NULL, Decoded::Null),
            (Value::HOLE, Decoded::Hole),
            (Value::UNINITIALIZED, Decoded::Uninitialized),
            (Value::boolean(false), Decoded::Boolean(false)),
            (Value::boolean(true), Decoded::Boolean(true)),
            (Value::int32(0), Decoded::Int32(0)),
            (Value::int32(u32::MAX), Decoded::Int32(u32::MAX)),
            (Value::int32(0x1234_5678), Decoded::Int32(0x1234_5678)),
            (Value::heap_ref(slot(1, 1)), Decoded::HeapRef(slot(1, 1))),
            (
                Value::heap_ref(slot(u16::MAX, u32::MAX)),
                Decoded::HeapRef(slot(u16::MAX, u32::MAX)),
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(value.decode(), Some(expected), "{value:?}");
            // Re-encoding the decoded meaning reproduces the exact bits.
            let reencoded = match expected {
                Decoded::Undefined => Value::UNDEFINED,
                Decoded::Null => Value::NULL,
                Decoded::Hole => Value::HOLE,
                Decoded::Uninitialized => Value::UNINITIALIZED,
                Decoded::Boolean(b) => Value::boolean(b),
                Decoded::Int32(v) => Value::int32(v),
                Decoded::HeapRef(id) => Value::heap_ref(id),
                Decoded::Number(x) => Value::number(x),
            };
            assert_eq!(reencoded.to_bits(), value.to_bits());
        }
    }

    #[test]
    fn numbers_are_not_boxed_and_roundtrip() {
        for x in [
            0.0f64,
            -0.0,
            1.5,
            -2.25,
            f64::MAX,
            f64::MIN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let value = Value::number(x);
            assert!(value.is_number(), "{x} should be an unboxed number");
            assert_eq!(value.decode(), Some(Decoded::Number(x)));
            assert_eq!(value.as_f64(), Some(x));
            assert_eq!(value.to_bits(), x.to_bits());
        }
    }

    #[test]
    fn every_nan_canonicalizes_and_stays_a_number() {
        for raw in [
            f64::NAN.to_bits(),
            0x7ff8_0000_0000_0001,
            0xffff_ffff_ffff_ffff,
            0x7ff0_0000_0000_0001, // signaling NaN
        ] {
            let value = Value::number(f64::from_bits(raw));
            assert_eq!(value.to_bits(), Value::CANON_NAN);
            assert!(value.is_number());
            match value.decode() {
                Some(Decoded::Number(x)) => assert!(x.is_nan()),
                other => panic!("expected NaN number, got {other:?}"),
            }
        }
    }

    #[test]
    fn canonical_nan_word_decodes_as_number_not_boxed() {
        // tag == 0 keeps CANON_NAN out of the boxed range.
        let value = Value::from_bits(Value::CANON_NAN);
        assert!(value.is_number());
        assert!(matches!(value.decode(), Some(Decoded::Number(_))));
    }

    #[test]
    fn malformed_boxed_payloads_are_rejected() {
        // int32 with a nonzero upper half.
        assert_eq!(
            Value::from_bits(boxed(TAG_INT32, 0x0001_0000_0000)).decode(),
            None
        );
        // undefined / null / hole / uninitialized with a nonzero payload.
        assert_eq!(Value::from_bits(boxed(TAG_UNDEFINED, 1)).decode(), None);
        assert_eq!(Value::from_bits(boxed(TAG_NULL, 1)).decode(), None);
        assert_eq!(Value::from_bits(boxed(TAG_HOLE, 1)).decode(), None);
        assert_eq!(Value::from_bits(boxed(TAG_UNINITIALIZED, 1)).decode(), None);
        // boolean payload outside {0, 1}.
        assert_eq!(Value::from_bits(boxed(TAG_BOOLEAN, 2)).decode(), None);
        // heapRef with a zero segment or zero slot.
        assert_eq!(
            Value::from_bits(boxed(TAG_HEAP_REF, 0x0000_0000_0001)).decode(),
            None
        );
        assert_eq!(
            Value::from_bits(boxed(TAG_HEAP_REF, 0x0001_0000_0000)).decode(),
            None
        );
    }

    #[test]
    fn distinct_tags_never_share_an_encoding() {
        // tags_disjoint: the same payload under different tags yields different words.
        let payload = 0u64;
        let tags = [
            TAG_HEAP_REF,
            TAG_INT32,
            TAG_UNDEFINED,
            TAG_NULL,
            TAG_BOOLEAN,
            TAG_HOLE,
            TAG_UNINITIALIZED,
        ];
        for (i, &left) in tags.iter().enumerate() {
            for &right in &tags[i + 1..] {
                assert_ne!(boxed(left, payload), boxed(right, payload));
            }
        }
    }

    #[test]
    fn slot_id_rejects_zero_parts() {
        assert!(SlotId::from_parts(0, 1).is_none());
        assert!(SlotId::from_parts(1, 0).is_none());
        assert!(SlotId::from_parts(0, 0).is_none());
        let id = slot(7, 9);
        assert_eq!(id.segment(), 7);
        assert_eq!(id.slot(), 9);
    }

    #[test]
    fn typed_accessors_agree_with_decode() {
        assert_eq!(Value::int32(42).as_int32(), Some(42));
        assert_eq!(Value::UNDEFINED.as_int32(), None);
        assert_eq!(Value::boolean(true).as_bool(), Some(true));
        assert_eq!(Value::boolean(false).as_bool(), Some(false));
        assert_eq!(Value::int32(1).as_bool(), None);
        let id = slot(3, 4);
        assert_eq!(Value::heap_ref(id).as_heap_ref(), Some(id));
        assert_eq!(Value::NULL.as_heap_ref(), None);
        assert!(Value::UNINITIALIZED.is_uninitialized());
        assert!(!Value::HOLE.is_uninitialized());
    }

    #[test]
    fn completion_tag_roundtrips_and_rejects_out_of_range() {
        for tag in [
            CompletionTag::Normal,
            CompletionTag::Throw,
            CompletionTag::Suspend,
            CompletionTag::FatalTrap,
        ] {
            assert_eq!(CompletionTag::from_u32(tag.as_u32()), Some(tag));
        }
        assert_eq!(CompletionTag::Normal.as_u32(), 0);
        assert_eq!(CompletionTag::FatalTrap.as_u32(), 3);
        assert_eq!(CompletionTag::from_u32(4), None);
        assert_eq!(CompletionTag::from_u32(u32::MAX), None);
    }

    #[test]
    fn shadow_frame_new_zeroes_padding_and_keeps_fields() {
        let mut register = Value::UNINITIALIZED;
        let frame = ShadowFrame::new(core::ptr::null_mut(), 12, 7, &mut register, 1);
        assert!(frame.previous.is_null());
        assert_eq!(frame.bytecode_pc, 12);
        assert_eq!(frame.module_id, 7);
        assert_eq!(frame.handle_len, 1);
        assert_eq!(frame._pad1, [0; 6]);
        assert!(core::ptr::eq(frame.handles, &raw mut register));
    }

    #[test]
    fn completion_wraps_value() {
        let completion = Completion::new(Value::int32(5));
        assert_eq!(completion.value.as_int32(), Some(5));
    }
}
