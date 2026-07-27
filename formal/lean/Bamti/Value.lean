import Std

namespace Bamti

/-- The concrete field widths of a NaN-boxed u64. -/
abbrev U16 := Fin 65536
abbrev U32 := Fin 4294967296
abbrev Header13 := Fin 8192
abbrev TagBits := Fin 8

/-- The two disjoint pieces of the low 48-bit payload. -/
structure Payload48 where
  upper : U16
  lower : U32

/-- Natural-number view used only to state the physical bit layout. -/
def payloadToNat (payload : Payload48) : Nat :=
  payload.upper.val * 4294967296 + payload.lower.val

def zeroU16 : U16 := ⟨0, by decide⟩
def zeroU32 : U32 := ⟨0, by decide⟩
def oneU32 : U32 := ⟨1, by decide⟩
def zeroPayload : Payload48 := ⟨zeroU16, zeroU32⟩
def onePayload : Payload48 := ⟨zeroU16, oneU32⟩

/-- A slot occupies the payload as `segment:u16 << 32 | slot:u32`.
Both fields are nonzero, as required for a valid heap reference. -/
structure SlotId where
  segment : U16
  slot : U32
  segment_nonzero : segment.val ≠ 0
  slot_nonzero : slot.val ≠ 0

def packSlot (id : SlotId) : Payload48 := ⟨id.segment, id.slot⟩

/-- Zero segments and slots are rejected rather than made into valid identities. -/
def unpackSlot (payload : Payload48) : Option SlotId :=
  if segmentZero : payload.upper.val = 0 then
    none
  else if slotZero : payload.lower.val = 0 then
    none
  else
    some
      { segment := payload.upper
        slot := payload.lower
        segment_nonzero := segmentZero
        slot_nonzero := slotZero }

/-- Splitting the 48-bit payload at bit 32 recovers the original u16/u32 pair. -/
theorem slot_pack_unpack (id : SlotId) : unpackSlot (packSlot id) = some id := by
  rcases id with ⟨segment, slot, segment_nonzero, slot_nonzero⟩
  simp [packSlot, unpackSlot, segment_nonzero, slot_nonzero]

/-- The seven nonzero NaN-box tags. -/
inductive Tag where
  | heapRef
  | int32
  | undefined
  | null
  | boolean
  | hole
  | uninitialized

def tagCode : Tag → Nat
  | .heapRef => 1
  | .int32 => 2
  | .undefined => 3
  | .null => 4
  | .boolean => 5
  | .hole => 6
  | .uninitialized => 7

def tagBits (tag : Tag) : TagBits :=
  ⟨tagCode tag, by cases tag <;> decide⟩

private theorem tagCode_pos (tag : Tag) : 0 < tagCode tag := by
  cases tag <;> decide

private theorem tagCode_injective : Function.Injective tagCode := by
  intro left right equal
  cases left <;> cases right <;> simp [tagCode] at equal ⊢

private theorem tagBits_nonzero (tag : Tag) : (tagBits tag).val ≠ 0 := by
  change tagCode tag ≠ 0
  exact Nat.ne_of_gt (tagCode_pos tag)

private theorem tagBits_injective : Function.Injective tagBits := by
  intro left right equal
  apply tagCode_injective
  have raw := congrArg Fin.val equal
  change tagCode left = tagCode right at raw
  exact raw

/-- A word is exactly `header:13 || tag:3 || payload:48`.
`toNat` is therefore the real 64-bit field algebra, not a lookup table. -/
structure Word64 where
  header : Header13
  tagField : TagBits
  payload : Payload48

def Word64.toNat (word : Word64) : Nat :=
  (word.header.val * 8 + word.tagField.val) * 281474976710656 + payloadToNat word.payload

/-- Header bits 63..51 for `0x7ff8_0000_0000_0000`. -/
def canonicalHeader : Header13 := ⟨4095, by decide⟩
def zeroTagBits : TagBits := ⟨0, by decide⟩

/-- The canonical positive quiet NaN. -/
def canonicalNaN : Word64 := ⟨canonicalHeader, zeroTagBits, zeroPayload⟩

/-- A boxed word has the quiet-NaN header and one of the seven nonzero tags. -/
def isBoxedWire (word : Word64) : Prop :=
  word.header = canonicalHeader ∧ word.tagField.val ≠ 0

private instance decidableIsBoxedWire (word : Word64) : Decidable (isBoxedWire word) := by
  unfold isBoxedWire
  infer_instance

/-- This is `CANON_NAN | (tag << 48) | payload` in field form. -/
def boxedWord (tag : Tag) (payload : Payload48) : Word64 :=
  ⟨canonicalHeader, tagBits tag, payload⟩

private theorem boxedWord_isBoxed (tag : Tag) (payload : Payload48) :
    isBoxedWire (boxedWord tag payload) := by
  constructor
  · rfl
  · exact tagBits_nonzero tag

/-- IEEE-754 exponent bits 62..52 and the 52-bit fraction split over the
header low bit, three tag bits, and 48 payload bits. -/
def exponentField (word : Word64) : Nat := (word.header.val / 2) % 2048

def isNaNWire (word : Word64) : Prop :=
  exponentField word = 2047 ∧
    (word.header.val % 2 = 1 ∨ word.tagField.val ≠ 0 ∨
      word.payload.upper.val ≠ 0 ∨ word.payload.lower.val ≠ 0)

private instance decidableIsNaNWire (word : Word64) : Decidable (isNaNWire word) := by
  unfold isNaNWire exponentField
  infer_instance

def canonicalizeNaN (word : Word64) : Word64 :=
  if isNaNWire word then canonicalNaN else word

/-- Every positive or negative IEEE-754 NaN pattern becomes the one permitted
arithmetic NaN word. -/
theorem canonical_nan (word : Word64) (is_nan : isNaNWire word) :
    canonicalizeNaN word = canonicalNaN := by
  simp [canonicalizeNaN, is_nan]

/-- Raw arithmetic words are admitted only outside the tagged NaN-box range. -/
structure NumberBits where
  wire : Word64
  not_boxed : ¬ isBoxedWire wire

/-- The source domain remains distinct from the wire domain. -/
inductive Value where
  | number (bits : NumberBits)
  | heapRef (id : SlotId)
  | int32 (bits : U32)
  | undefined
  | null
  | boolean (value : Bool)
  | hole
  | uninitialized

def int32Payload (bits : U32) : Payload48 := ⟨zeroU16, bits⟩
def boolPayload (value : Bool) : Payload48 := if value then onePayload else zeroPayload

/-- The encoder writes the actual header/tag/payload fields. -/
def encode : Value → Word64
  | .number bits => bits.wire
  | .heapRef id => boxedWord .heapRef (packSlot id)
  | .int32 bits => boxedWord .int32 (int32Payload bits)
  | .undefined => boxedWord .undefined zeroPayload
  | .null => boxedWord .null zeroPayload
  | .boolean value => boxedWord .boolean (boolPayload value)
  | .hole => boxedWord .hole zeroPayload
  | .uninitialized => boxedWord .uninitialized zeroPayload

/-- Tag-specific payload validation rejects malformed boxed words. -/
def decodeTagged (bits : TagBits) (payload : Payload48) : Option Value :=
  match bits.val with
  | 1 =>
      match unpackSlot payload with
      | some id => some (Value.heapRef id)
      | none => none
  | 2 =>
      if _upperZero : payload.upper.val = 0 then some (Value.int32 payload.lower) else none
  | 3 =>
      if _upperZero : payload.upper.val = 0 then
        if _lowerZero : payload.lower.val = 0 then some Value.undefined else none
      else none
  | 4 =>
      if _upperZero : payload.upper.val = 0 then
        if _lowerZero : payload.lower.val = 0 then some Value.null else none
      else none
  | 5 =>
      if _upperZero : payload.upper.val = 0 then
        if _lowerZero : payload.lower.val = 0 then some (Value.boolean false)
        else if _lowerOne : payload.lower.val = 1 then some (Value.boolean true) else none
      else none
  | 6 =>
      if _upperZero : payload.upper.val = 0 then
        if _lowerZero : payload.lower.val = 0 then some Value.hole else none
      else none
  | 7 =>
      if _upperZero : payload.upper.val = 0 then
        if _lowerZero : payload.lower.val = 0 then some Value.uninitialized else none
      else none
  | _ => none

/-- Tag-zero NaNs remain arithmetic words; malformed tagged payloads are rejected. -/
def decode (word : Word64) : Option Value :=
  if boxed : isBoxedWire word then
    decodeTagged word.tagField word.payload
  else
    some (Value.number ⟨word, boxed⟩)

private theorem decodeTagged_heapRef (id : SlotId) :
    decodeTagged (tagBits Tag.heapRef) (packSlot id) = some (Value.heapRef id) := by
  change
    (match unpackSlot (packSlot id) with
    | some found => some (Value.heapRef found)
    | none => none) = some (Value.heapRef id)
  rw [slot_pack_unpack]

private theorem decodeTagged_int32 (bits : U32) :
    decodeTagged (tagBits Tag.int32) (int32Payload bits) = some (Value.int32 bits) := by
  change
    (if _upperZero : zeroU16.val = 0 then some (Value.int32 bits) else none) =
      some (Value.int32 bits)
  simp [zeroU16]

private theorem decodeTagged_undefined :
    decodeTagged (tagBits Tag.undefined) zeroPayload = some Value.undefined := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : zeroU32.val = 0 then some Value.undefined else none
    else none) = some Value.undefined
  simp [zeroU16, zeroU32]

private theorem decodeTagged_null :
    decodeTagged (tagBits Tag.null) zeroPayload = some Value.null := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : zeroU32.val = 0 then some Value.null else none
    else none) = some Value.null
  simp [zeroU16, zeroU32]

private theorem decodeTagged_boolean_false :
    decodeTagged (tagBits Tag.boolean) (boolPayload false) = some (Value.boolean false) := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : zeroU32.val = 0 then some (Value.boolean false)
      else if _lowerOne : zeroU32.val = 1 then some (Value.boolean true) else none
    else none) = some (Value.boolean false)
  simp [zeroU16, zeroU32]

private theorem decodeTagged_boolean_true :
    decodeTagged (tagBits Tag.boolean) (boolPayload true) = some (Value.boolean true) := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : oneU32.val = 0 then some (Value.boolean false)
      else if _lowerOne : oneU32.val = 1 then some (Value.boolean true) else none
    else none) = some (Value.boolean true)
  simp [zeroU16, oneU32]

private theorem decodeTagged_hole :
    decodeTagged (tagBits Tag.hole) zeroPayload = some Value.hole := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : zeroU32.val = 0 then some Value.hole else none
    else none) = some Value.hole
  simp [zeroU16, zeroU32]

private theorem decodeTagged_uninitialized :
    decodeTagged (tagBits Tag.uninitialized) zeroPayload = some Value.uninitialized := by
  change
    (if _upperZero : zeroU16.val = 0 then
      if _lowerZero : zeroU32.val = 0 then some Value.uninitialized else none
    else none) = some Value.uninitialized
  simp [zeroU16, zeroU32]

/-- Encoding then decoding preserves every value representable by the actual
13/3/48-bit layout and its tag-specific payload rules. -/
theorem encode_decode (value : Value) : decode (encode value) = some value := by
  cases value with
  | number bits =>
      unfold encode decode
      rw [dif_neg bits.not_boxed]
  | heapRef id =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .heapRef (packSlot id))]
      exact decodeTagged_heapRef id
  | int32 bits =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .int32 (int32Payload bits))]
      exact decodeTagged_int32 bits
  | undefined =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .undefined zeroPayload)]
      exact decodeTagged_undefined
  | null =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .null zeroPayload)]
      exact decodeTagged_null
  | boolean value =>
      cases value with
      | false =>
          unfold encode decode
          rw [dif_pos (boxedWord_isBoxed .boolean (boolPayload false))]
          exact decodeTagged_boolean_false
      | true =>
          unfold encode decode
          rw [dif_pos (boxedWord_isBoxed .boolean (boolPayload true))]
          exact decodeTagged_boolean_true
  | hole =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .hole zeroPayload)]
      exact decodeTagged_hole
  | uninitialized =>
      unfold encode decode
      rw [dif_pos (boxedWord_isBoxed .uninitialized zeroPayload)]
      exact decodeTagged_uninitialized

/-- Distinct three-bit tags cannot share the same 48-bit payload encoding. -/
theorem tags_disjoint (left right : Tag) (payload : Payload48) (different : left ≠ right) :
    boxedWord left payload ≠ boxedWord right payload := by
  intro same
  have same_bits : tagBits left = tagBits right := congrArg Word64.tagField same
  exact different (tagBits_injective same_bits)

end Bamti
