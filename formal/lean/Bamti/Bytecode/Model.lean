import Bamti.Abi

namespace Bamti.Bytecode

/-- The decoder models the one-byte, minimal-LEB fragment used by this bounded proof. -/
def FieldLimit : Nat := 128
abbrev EncodedField := List Nat
abbrev EncodedInstr := List EncodedField
abbrev EncodedHandler := List EncodedField

inductive DecodeError where
  | badMagic
  | unsupportedVersion
  | malformedInteger
  | nonCanonicalInteger
  | invalidOpcode
  | malformedInstruction
  | malformedHandler
  deriving DecidableEq, Repr

inductive Op where
  | load
  | add
  | jump
  | suspend
  | halt
  deriving DecidableEq, Repr

inductive Instr where
  | load (dst : Nat)
  | add (dst left right : Nat)
  | jump (target : Nat)
  | suspend (resumePc : Nat)
  | halt
  deriving DecidableEq, Repr

namespace Instr

def op : Instr → Op
  | .load _ => .load
  | .add _ _ _ => .add
  | .jump _ => .jump
  | .suspend _ => .suspend
  | .halt => .halt

def reads : Instr → List Nat
  | .load _ => []
  | .add _ left right => [left, right]
  | .jump _ => []
  | .suspend _ => []
  | .halt => []

def writes : Instr → List Nat
  | .load dst => [dst]
  | .add dst _ _ => [dst]
  | .jump _ => []
  | .suspend _ => []
  | .halt => []

def target? : Instr → Option Nat
  | .jump target => some target
  | .suspend resumePc => some resumePc
  | _ => none

end Instr

structure Handler where
  startPc : Nat
  endPc : Nat
  handlerPc : Nat
  deriving DecidableEq, Repr

structure Program where
  code : List Instr
  handlers : List Handler
  deriving DecidableEq, Repr

inductive DecodeResult where
  | accepted (program : Program)
  | rejected (error : DecodeError)
  deriving DecidableEq, Repr

structure Wire where
  magic : List Nat
  version : EncodedField
  instructions : List EncodedInstr
  handlers : List EncodedHandler
  deriving DecidableEq, Repr

/-- Raw fields must have exactly one 7-bit group; extra groups are non-minimal. -/
def decodeField : EncodedField → Except DecodeError Nat
  | [value] =>
      if value < FieldLimit then .ok value else .error .malformedInteger
  | [] => .error .malformedInteger
  | _ => .error .nonCanonicalInteger

def encodeField (value : Nat) : EncodedField := [value]

def encodeOpcode : Op → EncodedField
  | .load => encodeField 0
  | .add => encodeField 1
  | .jump => encodeField 2
  | .suspend => encodeField 3
  | .halt => encodeField 4

def decodeOpcode (field : EncodedField) : Except DecodeError Op :=
  match decodeField field with
  | .error error => .error error
  | .ok 0 => .ok .load
  | .ok 1 => .ok .add
  | .ok 2 => .ok .jump
  | .ok 3 => .ok .suspend
  | .ok 4 => .ok .halt
  | .ok _ => .error .invalidOpcode

def encodeInstr : Instr → EncodedInstr
  | .load dst => [encodeOpcode .load, encodeField dst]
  | .add dst left right => [encodeOpcode .add, encodeField dst, encodeField left, encodeField right]
  | .jump target => [encodeOpcode .jump, encodeField target]
  | .suspend resumePc => [encodeOpcode .suspend, encodeField resumePc]
  | .halt => [encodeOpcode .halt]

def decodeInstr : EncodedInstr → Except DecodeError Instr
  | opcode :: operands =>
      match decodeOpcode opcode with
      | .error error => .error error
      | .ok .load =>
          match operands with
          | [dst] =>
              match decodeField dst with
              | .ok value => .ok (.load value)
              | .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .add =>
          match operands with
          | [dst, left, right] =>
              match decodeField dst, decodeField left, decodeField right with
              | .ok dstValue, .ok leftValue, .ok rightValue => .ok (.add dstValue leftValue rightValue)
              | .error error, _, _ => .error error
              | _, .error error, _ => .error error
              | _, _, .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .jump =>
          match operands with
          | [target] =>
              match decodeField target with
              | .ok value => .ok (.jump value)
              | .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .suspend =>
          match operands with
          | [resumePc] =>
              match decodeField resumePc with
              | .ok value => .ok (.suspend value)
              | .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .halt =>
          match operands with
          | [] => .ok .halt
          | _ => .error .malformedInstruction
  | [] => .error .malformedInstruction

def decodeInstructions : List EncodedInstr → Except DecodeError (List Instr)
  | [] => .ok []
  | encoded :: rest =>
      match decodeInstr encoded, decodeInstructions rest with
      | .ok instruction, .ok instructions => .ok (instruction :: instructions)
      | .error error, _ => .error error
      | _, .error error => .error error

def encodeHandler (handler : Handler) : EncodedHandler :=
  [encodeField handler.startPc, encodeField handler.endPc, encodeField handler.handlerPc]

def decodeHandler : EncodedHandler → Except DecodeError Handler
  | [startPc, endPc, handlerPc] =>
      match decodeField startPc, decodeField endPc, decodeField handlerPc with
      | .ok startValue, .ok endValue, .ok handlerValue =>
          .ok ⟨startValue, endValue, handlerValue⟩
      | .error error, _, _ => .error error
      | _, .error error, _ => .error error
      | _, _, .error error => .error error
  | _ => .error .malformedHandler

def decodeHandlers : List EncodedHandler → Except DecodeError (List Handler)
  | [] => .ok []
  | encoded :: rest =>
      match decodeHandler encoded, decodeHandlers rest with
      | .ok handler, .ok handlers => .ok (handler :: handlers)
      | .error error, _ => .error error
      | _, .error error => .error error

def magicBytes : List Nat := [66, 77, 84, 66, 67, 0, 0, 1]
def formatVersion : Nat := 2

def encode (program : Program) : Wire :=
  { magic := magicBytes
    version := encodeField formatVersion
    instructions := program.code.map encodeInstr
    handlers := program.handlers.map encodeHandler }

def decode (wire : Wire) : DecodeResult :=
  if wire.magic ≠ magicBytes then .rejected .badMagic
  else
    match decodeField wire.version with
    | .error error => .rejected error
    | .ok version =>
        if version ≠ formatVersion then .rejected .unsupportedVersion
        else
          match decodeInstructions wire.instructions, decodeHandlers wire.handlers with
          | .ok code, .ok handlers => .accepted ⟨code, handlers⟩
          | .error error, _ => .rejected error
          | _, .error error => .rejected error

theorem decode_rejects_bad_magic (wire : Wire) (badMagic : wire.magic ≠ magicBytes) :
    decode wire = .rejected .badMagic := by
  simp [decode, badMagic]

def instructionCanonical : Instr → Prop
  | .load dst => dst < FieldLimit
  | .add dst left right => dst < FieldLimit ∧ left < FieldLimit ∧ right < FieldLimit
  | .jump target => target < FieldLimit
  | .suspend resumePc => resumePc < FieldLimit
  | .halt => True

def handlerCanonical (handler : Handler) : Prop :=
  handler.startPc < FieldLimit ∧ handler.endPc < FieldLimit ∧ handler.handlerPc < FieldLimit

def programCanonical (program : Program) : Prop :=
  (∀ instruction ∈ program.code, instructionCanonical instruction) ∧
  ∀ handler ∈ program.handlers, handlerCanonical handler

def wireCanonical (wire : Wire) : Prop :=
  ∃ program, programCanonical program ∧ wire = encode program

/-- Instruction boundaries are decoded structure, never a caller-supplied target list. -/
def instructionBoundaries (program : Program) : List Nat := List.range (program.code.length + 1)
def boundary (program : Program) (pc : Nat) : Prop := pc ∈ instructionBoundaries program

def controlTargets : List Instr → List Nat
  | [] => []
  | instruction :: rest =>
      match instruction.target? with
      | some target => target :: controlTargets rest
      | none => controlTargets rest

def targets (program : Program) : List Nat := controlTargets program.code

def backEdgeTargetsFrom (pc : Nat) : List Instr → List Nat
  | [] => []
  | .jump target :: rest =>
      if target ≤ pc then target :: backEdgeTargetsFrom (pc + 1) rest
      else backEdgeTargetsFrom (pc + 1) rest
  | _ :: rest => backEdgeTargetsFrom (pc + 1) rest

def backEdgeTargets (program : Program) : List Nat := backEdgeTargetsFrom 0 program.code

def suspendResumePcs : List Instr → List Nat
  | [] => []
  | .suspend resumePc :: rest => resumePc :: suspendResumePcs rest
  | _ :: rest => suspendResumePcs rest

def entryCandidates (program : Program) : List Nat :=
  0 :: (backEdgeTargets program ++ suspendResumePcs program.code)

/-- Filtering the ordered instruction-boundary list gives sorted, duplicate-free entry ordinals. -/
def entryPoints (program : Program) : List Nat :=
  (List.range program.code.length).filter fun pc => decide (pc ∈ entryCandidates program)

def nextPc (pc : Nat) : Instr → Option Nat
  | .load _ => some (pc + 1)
  | .add _ _ _ => some (pc + 1)
  | .jump target => some target
  | .suspend resumePc => some resumePc
  | .halt => none

theorem decodeField_rejects_noncanonical (first second : Nat) :
    decodeField [first, second] = .error .nonCanonicalInteger := rfl

theorem decodeField_encodeField (value : Nat) (h : value < FieldLimit) :
    decodeField (encodeField value) = .ok value := by
  simp [decodeField, encodeField, h]

theorem decodeOpcode_encodeOpcode (opcode : Op) :
    decodeOpcode (encodeOpcode opcode) = .ok opcode := by
  cases opcode <;> simp [decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit]

theorem decodeInstr_encodeInstr (instruction : Instr) (h : instructionCanonical instruction) :
    decodeInstr (encodeInstr instruction) = .ok instruction := by
  cases instruction with
  | load dst =>
      simp only [instructionCanonical] at h
      have dstBound : dst < 128 := by simpa [FieldLimit] using h
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit,
        dstBound]
  | add dst left right =>
      rcases h with ⟨dstCanonical, leftCanonical, rightCanonical⟩
      have dstBound : dst < 128 := by simpa [FieldLimit] using dstCanonical
      have leftBound : left < 128 := by simpa [FieldLimit] using leftCanonical
      have rightBound : right < 128 := by simpa [FieldLimit] using rightCanonical
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit,
        dstBound, leftBound, rightBound]
  | jump target =>
      simp only [instructionCanonical] at h
      have targetBound : target < 128 := by simpa [FieldLimit] using h
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit,
        targetBound]
  | suspend resumePc =>
      simp only [instructionCanonical] at h
      have resumeBound : resumePc < 128 := by simpa [FieldLimit] using h
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit,
        resumeBound]
  | halt =>
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField, FieldLimit]

theorem decodeHandler_encodeHandler (handler : Handler) (h : handlerCanonical handler) :
    decodeHandler (encodeHandler handler) = .ok handler := by
  rcases handler with ⟨startPc, endPc, handlerPc⟩
  rcases h with ⟨startCanonical, endCanonical, handlerCanonical⟩
  have startBound : startPc < 128 := by simpa [FieldLimit] using startCanonical
  have endBound : endPc < 128 := by simpa [FieldLimit] using endCanonical
  have handlerBound : handlerPc < 128 := by simpa [FieldLimit] using handlerCanonical
  simp [encodeHandler, decodeHandler, decodeField, encodeField, FieldLimit,
    startBound, endBound, handlerBound]

theorem decodeInstructions_encodeInstructions (code : List Instr)
    (h : ∀ instruction ∈ code, instructionCanonical instruction) :
    decodeInstructions (code.map encodeInstr) = .ok code := by
  induction code with
  | nil => rfl
  | cons instruction rest ih =>
      have hInstruction : instructionCanonical instruction := h instruction (by simp)
      have hRest : ∀ remaining ∈ rest, instructionCanonical remaining := by
        intro remaining hRemaining
        exact h remaining (by simp [hRemaining])
      simp [decodeInstructions, decodeInstr_encodeInstr instruction hInstruction, ih hRest]

theorem decodeHandlers_encodeHandlers (handlers : List Handler)
    (h : ∀ handler ∈ handlers, handlerCanonical handler) :
    decodeHandlers (handlers.map encodeHandler) = .ok handlers := by
  induction handlers with
  | nil => rfl
  | cons handler rest ih =>
      have hHandler : handlerCanonical handler := h handler (by simp)
      have hRest : ∀ remaining ∈ rest, handlerCanonical remaining := by
        intro remaining hRemaining
        exact h remaining (by simp [hRemaining])
      simp [decodeHandlers, decodeHandler_encodeHandler handler hHandler, ih hRest]

end Bamti.Bytecode
