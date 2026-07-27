import Bamti.Value

namespace Bamti

/-- Real ABI bytes range over all 256 octets. -/
abbrev Byte := Fin 256
abbrev Payload7 := Fin 128
abbrev Payload4 := Fin 16

def byteNat (byte : Byte) : Nat := byte.val

def byteOf (value : Nat) : Byte :=
  ⟨value % 256, by
    apply Nat.mod_lt
    omega⟩

/-- The `index`th little-endian byte of an unsigned field. -/
def littleByte (word index : Nat) : Byte := byteOf (word / 256 ^ index)

/-- Low seven payload bits with the continuation bit set or clear. -/
def continuation (payload : Payload7) : Byte :=
  ⟨128 + payload.val, by
    have hpayload : payload.val < 128 := payload.isLt
    omega⟩

def terminal (payload : Payload7) : Byte :=
  ⟨payload.val, by
    have hpayload : payload.val < 128 := payload.isLt
    omega⟩

def terminal4 (payload : Payload4) : Byte :=
  ⟨payload.val, by
    have hpayload : payload.val < 16 := payload.isLt
    omega⟩

def low7 (byte : Byte) : Payload7 :=
  ⟨byte.val % 128, by
    apply Nat.mod_lt
    omega⟩

def low4 (byte : Byte) : Payload4 :=
  ⟨byte.val % 16, by
    apply Nat.mod_lt
    omega⟩

def zero7 : Payload7 := ⟨0, by omega⟩
def zero4 : Payload4 := ⟨0, by omega⟩

private theorem continuation_has_bit (payload : Payload7) :
    128 ≤ (continuation payload).val := by
  change 128 ≤ 128 + payload.val
  omega

private theorem terminal_is_final (payload : Payload7) :
    (terminal payload).val < 128 := by
  change payload.val < 128
  exact payload.isLt

private theorem terminal4_fits_u32 (payload : Payload4) :
    (terminal4 payload).val < 16 := by
  change payload.val < 16
  exact payload.isLt

private theorem low7_continuation (payload : Payload7) :
    low7 (continuation payload) = payload := by
  apply Fin.ext
  change (128 + payload.val) % 128 = payload.val
  have hpayload : payload.val < 128 := payload.isLt
  omega

private theorem low7_terminal (payload : Payload7) : low7 (terminal payload) = payload := by
  apply Fin.ext
  change payload.val % 128 = payload.val
  exact Nat.mod_eq_of_lt payload.isLt

private theorem low4_terminal4 (payload : Payload4) : low4 (terminal4 payload) = payload := by
  apply Fin.ext
  change payload.val % 16 = payload.val
  exact Nat.mod_eq_of_lt payload.isLt

/-- A canonical unsigned LEB128 value. Constructors represent the exact seven-bit
payload groups; widths above one require a nonzero highest group. The fifth group
is four bits, enforcing the u32 ceiling. -/
inductive Leb128 where
  | one (p0 : Payload7)
  | two (p0 p1 : Payload7) (highest_nonzero : p1.val ≠ 0)
  | three (p0 p1 p2 : Payload7) (highest_nonzero : p2.val ≠ 0)
  | four (p0 p1 p2 p3 : Payload7) (highest_nonzero : p3.val ≠ 0)
  | five (p0 p1 p2 p3 : Payload7) (p4 : Payload4) (highest_nonzero : p4.val ≠ 0)

/-- The mathematical u32 denoted by the LEB groups. -/
def Leb128.toU32 : Leb128 → U32
  | .one p0 => ⟨p0.val, by
      have hp0 : p0.val < 128 := p0.isLt
      omega⟩
  | .two p0 p1 _ => ⟨p0.val + p1.val * 128, by
      have hp0 : p0.val < 128 := p0.isLt
      have hp1 : p1.val < 128 := p1.isLt
      omega⟩
  | .three p0 p1 p2 _ => ⟨p0.val + p1.val * 128 + p2.val * 16384, by
      have hp0 : p0.val < 128 := p0.isLt
      have hp1 : p1.val < 128 := p1.isLt
      have hp2 : p2.val < 128 := p2.isLt
      omega⟩
  | .four p0 p1 p2 p3 _ => ⟨p0.val + p1.val * 128 + p2.val * 16384 + p3.val * 2097152, by
      have hp0 : p0.val < 128 := p0.isLt
      have hp1 : p1.val < 128 := p1.isLt
      have hp2 : p2.val < 128 := p2.isLt
      have hp3 : p3.val < 128 := p3.isLt
      omega⟩
  | .five p0 p1 p2 p3 p4 _ =>
      ⟨p0.val + p1.val * 128 + p2.val * 16384 + p3.val * 2097152 + p4.val * 268435456, by
        have hp0 : p0.val < 128 := p0.isLt
        have hp1 : p1.val < 128 := p1.isLt
        have hp2 : p2.val < 128 := p2.isLt
        have hp3 : p3.val < 128 := p3.isLt
        have hp4 : p4.val < 16 := p4.isLt
        omega⟩

/-- Emits continuation bits on all but the terminal byte. -/
def leEncode : Leb128 → List Byte
  | .one p0 => [terminal p0]
  | .two p0 p1 _ => [continuation p0, terminal p1]
  | .three p0 p1 p2 _ => [continuation p0, continuation p1, terminal p2]
  | .four p0 p1 p2 p3 _ =>
      [continuation p0, continuation p1, continuation p2, terminal p3]
  | .five p0 p1 p2 p3 p4 _ =>
      [continuation p0, continuation p1, continuation p2, continuation p3, terminal4 p4]

/-- Strict u32 LEB128 decoding. It rejects a missing continuation byte, a final
byte with its continuation bit set, a nonminimal zero highest group, and a fifth
payload group wider than four bits. -/
def leDecode : List Byte → Option Leb128
  | [a] =>
      if _final : a.val < 128 then some (.one (low7 a)) else none
  | [a, b] =>
      if _aContinues : 128 ≤ a.val then
        if _bFinal : b.val < 128 then
          let high := low7 b
          if minimal : high.val ≠ 0 then some (.two (low7 a) high minimal) else none
        else none
      else none
  | [a, b, c] =>
      if _aContinues : 128 ≤ a.val then
        if _bContinues : 128 ≤ b.val then
          if _cFinal : c.val < 128 then
            let high := low7 c
            if minimal : high.val ≠ 0 then
              some (.three (low7 a) (low7 b) high minimal)
            else none
          else none
        else none
      else none
  | [a, b, c, d] =>
      if _aContinues : 128 ≤ a.val then
        if _bContinues : 128 ≤ b.val then
          if _cContinues : 128 ≤ c.val then
            if _dFinal : d.val < 128 then
              let high := low7 d
              if minimal : high.val ≠ 0 then
                some (.four (low7 a) (low7 b) (low7 c) high minimal)
              else none
            else none
          else none
        else none
      else none
  | [a, b, c, d, e] =>
      if _aContinues : 128 ≤ a.val then
        if _bContinues : 128 ≤ b.val then
          if _cContinues : 128 ≤ c.val then
            if _dContinues : 128 ≤ d.val then
              if _eFitsU32 : e.val < 16 then
                let high := low4 e
                if minimal : high.val ≠ 0 then
                  some (.five (low7 a) (low7 b) (low7 c) (low7 d) high minimal)
                else none
              else none
            else none
          else none
        else none
      else none
  | _ => none

private theorem leDecode_encode (value : Leb128) :
    leDecode (leEncode value) = some value := by
  cases value with
  | one p0 =>
      simp [leEncode, leDecode, terminal_is_final, low7_terminal]
  | two p0 p1 highest_nonzero =>
      simp [leEncode, leDecode, continuation_has_bit, terminal_is_final,
        low7_continuation, low7_terminal, highest_nonzero]
  | three p0 p1 p2 highest_nonzero =>
      simp [leEncode, leDecode, continuation_has_bit, terminal_is_final,
        low7_continuation, low7_terminal, highest_nonzero]
  | four p0 p1 p2 p3 highest_nonzero =>
      simp [leEncode, leDecode, continuation_has_bit, terminal_is_final,
        low7_continuation, low7_terminal, highest_nonzero]
  | five p0 p1 p2 p3 p4 highest_nonzero =>
      simp [leEncode, leDecode, continuation_has_bit, terminal4_fits_u32,
        low7_continuation, low4_terminal4, highest_nonzero]

private theorem reject_nonminimal_two (p0 : Payload7) :
    leDecode [continuation p0, terminal zero7] = none := by
  simp [leDecode, continuation_has_bit, terminal_is_final, low7_terminal, zero7]

private theorem reject_nonminimal_three (p0 p1 : Payload7) :
    leDecode [continuation p0, continuation p1, terminal zero7] = none := by
  simp [leDecode, continuation_has_bit, terminal_is_final, low7_terminal, zero7]

private theorem reject_nonminimal_four (p0 p1 p2 : Payload7) :
    leDecode [continuation p0, continuation p1, continuation p2, terminal zero7] = none := by
  simp [leDecode, continuation_has_bit, terminal_is_final, low7_terminal, zero7]

private theorem reject_nonminimal_five (p0 p1 p2 p3 : Payload7) :
    leDecode [continuation p0, continuation p1, continuation p2, continuation p3, terminal4 zero4] =
      none := by
  simp [leDecode, continuation_has_bit, terminal4_fits_u32, low4_terminal4, zero4]

/-- The encoder round-trips through the strict decoder, and every overlong form
whose highest payload group is zero is rejected at each permitted width. -/
theorem le_roundtrip (value : Leb128) :
    leDecode (leEncode value) = some value ∧
      (∀ p0 : Payload7, leDecode [continuation p0, terminal zero7] = none) ∧
      (∀ p0 p1 : Payload7,
        leDecode [continuation p0, continuation p1, terminal zero7] = none) ∧
      (∀ p0 p1 p2 : Payload7,
        leDecode [continuation p0, continuation p1, continuation p2, terminal zero7] = none) ∧
      (∀ p0 p1 p2 p3 : Payload7,
        leDecode [continuation p0, continuation p1, continuation p2, continuation p3,
          terminal4 zero4] = none) := by
  refine ⟨leDecode_encode value, ?_, ?_, ?_, ?_⟩
  · intro p0
    exact reject_nonminimal_two p0
  · intro p0 p1
    exact reject_nonminimal_three p0 p1
  · intro p0 p1 p2
    exact reject_nonminimal_four p0 p1 p2
  · intro p0 p1 p2 p3
    exact reject_nonminimal_five p0 p1 p2 p3

/-- Fixed-width ABI fields are serialized little-endian. -/
def u16LE (word : U16) : List Byte :=
  [littleByte word.val 0, littleByte word.val 1]

def u32LE (word : U32) : List Byte :=
  [littleByte word.val 0, littleByte word.val 1, littleByte word.val 2, littleByte word.val 3]

def u64LE (word : Word64) : List Byte :=
  [littleByte (Word64.toNat word) 0, littleByte (Word64.toNat word) 1,
    littleByte (Word64.toNat word) 2, littleByte (Word64.toNat word) 3,
    littleByte (Word64.toNat word) 4, littleByte (Word64.toNat word) 5,
    littleByte (Word64.toNat word) 6, littleByte (Word64.toNat word) 7]

def zeroByte : Byte := ⟨0, by omega⟩
def padding4 : List Byte := [zeroByte, zeroByte, zeroByte, zeroByte]
def padding6 : List Byte := [zeroByte, zeroByte, zeroByte, zeroByte, zeroByte, zeroByte]

/-- The native frame stores pointers as modeled 64-bit words and uses a u32 PC
and u16 handle length exactly as specified by the shared executor ABI. -/
structure ShadowFrame where
  previous : Word64
  bytecodePc : U32
  handles : Word64
  handleLen : U16

/-- Bytes 0..31 of `ShadowFrame`: 8 + (4 + 4 pad) + 8 + (2 + 6 pad). -/
def shadowFrameBytes (frame : ShadowFrame) : List Byte :=
  u64LE frame.previous ++
    u32LE frame.bytecodePc ++
      padding4 ++
        u64LE frame.handles ++
          u16LE frame.handleLen ++ padding6

/-- Every field occupies its required byte range in the 32-byte, 8-aligned
shadow-frame header; the padding bytes are physically zero. -/
theorem shadow_frame_layout (frame : ShadowFrame) :
    (shadowFrameBytes frame).length = 32 ∧
      (shadowFrameBytes frame).take 8 = u64LE frame.previous ∧
      ((shadowFrameBytes frame).drop 8).take 4 = u32LE frame.bytecodePc ∧
      ((shadowFrameBytes frame).drop 12).take 4 = padding4 ∧
      ((shadowFrameBytes frame).drop 16).take 8 = u64LE frame.handles ∧
      ((shadowFrameBytes frame).drop 24).take 2 = u16LE frame.handleLen ∧
      ((shadowFrameBytes frame).drop 26).take 6 = padding6 := by
  simp [shadowFrameBytes, u64LE, u32LE, u16LE, padding4, padding6]

/-- A generation is an actual u32 value. The largest value is retained, rather
than incremented modulo 2^32. -/
def generationMax : Nat := 4294967295

structure Generation where
  value : Nat
  bound : value ≤ generationMax

inductive GenerationStep where
  | advanced (generation : Generation)
  | exhausted

def nextGeneration (current : Generation) : GenerationStep :=
  if maxed : current.value = generationMax then
    .exhausted
  else
    .advanced ⟨current.value + 1, by
      have hbound : current.value ≤ 4294967295 := current.bound
      have hnot : current.value ≠ 4294967295 := by
        simpa [generationMax] using maxed
      change current.value + 1 ≤ 4294967295
      omega⟩

inductive SlotStatus where
  | live (generation : Generation)
  | retired (generation : Generation)
  | exhausted

structure FatHandle where
  slot : SlotId
  generation : Generation

structure SlotEntry where
  slot : SlotId
  status : SlotStatus

/-- A handle resolves only while its slot is live at the exact generation. -/
def resolves (entry : SlotEntry) (handle : FatHandle) : Prop :=
  match entry.status with
  | .live generation => entry.slot = handle.slot ∧ generation = handle.generation
  | .retired _ => False
  | .exhausted => False

/-- Retirement removes a slot from the live resolution domain. Reaching u32::MAX
makes it permanently exhausted instead of wrapping to generation zero. -/
def retire (entry : SlotEntry) : SlotEntry :=
  match entry.status with
  | .live generation =>
      match nextGeneration generation with
      | .advanced next => { entry with status := .retired next }
      | .exhausted => { entry with status := .exhausted }
  | .retired _ => entry
  | .exhausted => entry

private theorem nextGeneration_cases (current : Generation) :
    nextGeneration current = .exhausted ∨
      ∃ next, nextGeneration current = .advanced next ∧ next.value = current.value + 1 := by
  by_cases maxed : current.value = generationMax
  · left
    simp [nextGeneration, maxed]
  · right
    let next : Generation := ⟨current.value + 1, by
      have hbound : current.value ≤ 4294967295 := current.bound
      have hnot : current.value ≠ 4294967295 := by
        simpa [generationMax] using maxed
      change current.value + 1 ≤ 4294967295
      omega⟩
    refine ⟨next, ?_, ?_⟩
    · simp [nextGeneration, maxed, next]
    · simp [next]

/-- A resolved handle becomes unresolvable after retirement, and a non-exhausted
retirement advances exactly the generation carried by that handle. -/
theorem stale_after_retire (entry : SlotEntry) (handle : FatHandle)
    (resolved : resolves entry handle) :
    ¬ resolves (retire entry) handle ∧
      ((retire entry).status = .exhausted ∨
        ∃ next, (retire entry).status = .retired next ∧
          next.value = handle.generation.value + 1) := by
  cases entry with
  | mk slot status =>
      cases status with
      | live generation =>
          change slot = handle.slot ∧ generation = handle.generation at resolved
          rcases resolved with ⟨_, generation_eq⟩
          rcases nextGeneration_cases generation with exhausted | ⟨next, advanced, next_value⟩
          · constructor
            · simp [retire, exhausted, resolves]
            · left
              simp [retire, exhausted]
          · constructor
            · simp [retire, advanced, resolves]
            · right
              refine ⟨next, ?_, ?_⟩
              · simp [retire, advanced]
              · simpa [generation_eq] using next_value
      | retired generation =>
          simp [resolves] at resolved
      | exhausted =>
          simp [resolves] at resolved

/-- A u32 generation either exhausts exactly at `u32::MAX`, or advances inside
the u32 range to a nonzero successor. No transition wraps to zero. -/
theorem generation_never_wraps (current : Generation) :
    (nextGeneration current = .exhausted ↔ current.value = generationMax) ∧
      ∀ next, nextGeneration current = .advanced next →
        next.value = current.value + 1 ∧ next.value ≤ generationMax ∧ next.value ≠ 0 := by
  by_cases maxed : current.value = generationMax
  · constructor
    · simp [nextGeneration, maxed]
    · intro next advanced
      simp [nextGeneration, maxed] at advanced
  · constructor
    · simp [nextGeneration, maxed]
    · intro next advanced
      simp [nextGeneration, maxed] at advanced
      subst next
      constructor
      · rfl
      constructor
      · have hbound : current.value ≤ 4294967295 := current.bound
        have hnot : current.value ≠ 4294967295 := by
          simpa [generationMax] using maxed
        change current.value + 1 ≤ 4294967295
        omega
      · change current.value + 1 ≠ 0
        omega

end Bamti
