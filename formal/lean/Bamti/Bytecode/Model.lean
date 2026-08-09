import Bamti.Abi

namespace Bamti.Bytecode

/-- The decoder models the one-byte, minimal-LEB fragment used by this bounded proof. -/
def FieldLimit : Nat := 128

/-- Embedded module blobs and the outer program envelope have distinct v4 magic headers. -/
def moduleMagicBytes : List Nat := [66, 77, 84, 66, 67, 0, 0, 1]
def programMagicBytes : List Nat := [66, 77, 84, 80, 67, 0, 0, 1]
def magicBytes : List Nat := programMagicBytes
def formatVersion : Nat := 4
abbrev EncodedField := List Nat
abbrev EncodedInstr := List EncodedField
abbrev EncodedHandler := List EncodedField
abbrev EncodedConstant := List Nat

inductive DecodeError where
  | badMagic
  | unsupportedVersion
  | malformedInteger
  | nonCanonicalInteger
  | invalidOpcode
  | invalidBinaryOperator
  | malformedInstruction
  | malformedHandler
  | malformedFunction
  | malformedConstant
  | malformedModule
  deriving DecidableEq, Repr

/-- The production opcode tags used by the bounded instruction subset below. -/
inductive Op where
  | loadConst
  | binary
  | jump
  | suspend
  | halt
  | createCell
  deriving DecidableEq, Repr

/--
The proof intentionally models a bounded subset of the production v4 algebra,
but its constructors and tags are the production ones rather than the retired
five-op wire.
-/
inductive Instr where
  | loadConst (dst constant : Nat)
  | binary (dst left right : Nat)
  | jump (target : Nat)
  | suspend (dst src resumePc : Nat)
  | halt
  | createCell (dst : Nat)
  deriving DecidableEq, Repr

namespace Instr

def op : Instr → Op
  | .loadConst _ _ => .loadConst
  | .binary _ _ _ => .binary
  | .jump _ => .jump
  | .suspend _ _ _ => .suspend
  | .halt => .halt
  | .createCell _ => .createCell

def reads : Instr → List Nat
  | .loadConst _ _ => []
  | .binary _ left right => [left, right]
  | .jump _ => []
  | .suspend _ src _ => [src]
  | .halt => []
  | .createCell _ => []

def writes : Instr → List Nat
  | .loadConst dst _ => [dst]
  | .binary dst _ _ => [dst]
  | .jump _ => []
  | .suspend dst _ _ => [dst]
  | .halt => []
  | .createCell dst => [dst]

def target? : Instr → Option Nat
  | .jump target => some target
  | .suspend _ _ resumePc => some resumePc
  | _ => none

end Instr

structure Handler where
  startPc : Nat
  endPc : Nat
  handlerPc : Nat
  deriving DecidableEq, Repr

/-- One production function body: a code list plus exception handlers. -/
structure Function where
  code : List Instr
  handlers : List Handler
  deriving DecidableEq, Repr

abbrev FunctionBody := Function

/-- Persistable v4 constant-pool values; runtime identities are intentionally absent. -/
inductive Constant where
  | numberBits (bits : Nat)
  | int32 (value : Int)
  | string (units : List Nat)
  | boolean (value : Bool)
  | null
  | undefined
  | bigint (text : List Nat)
  deriving DecidableEq, Repr

/-- Program-only linkage metadata attached to a canonical module blob. -/
inductive EdgeTarget where
  | local (moduleId : Nat)
  | external
  deriving DecidableEq, Repr

inductive EdgeKind where
  | static
  | dynamic
  | staticAndDynamic
  deriving DecidableEq, Repr

structure Edge where
  specifier : Nat
  target : EdgeTarget
  kind : EdgeKind
  deriving DecidableEq, Repr

inductive BindingKind where
  | hoisted
  | lexical
  | imported (edge name : Nat)
  | namespace (edge : Nat)
  deriving DecidableEq, Repr

structure Binding where
  name : Nat
  kind : BindingKind
  deriving DecidableEq, Repr

inductive ExportSource where
  | local (binding : Nat)
  | indirect (edge name : Nat)
  deriving DecidableEq, Repr

structure Export where
  name : Nat
  source : ExportSource
  deriving DecidableEq, Repr

structure Module where
  constants : List Constant
  functions : List Function
  entry : Nat
  deriving DecidableEq, Repr

structure ProgramModule where
  name : Nat
  code : Module
  edges : List Edge
  bindings : List Binding
  exports : List Export
  deriving DecidableEq, Repr

/-- The v4 self-contained executable envelope. -/
structure ProgramEnvelope where
  modules : List ProgramModule
  entry : Nat
  deriving DecidableEq, Repr

structure EncodedFunction where
  instructions : List EncodedInstr
  handlers : List EncodedHandler
  deriving DecidableEq, Repr

structure EncodedModule where
  magic : List Nat
  version : EncodedField
  constants : List EncodedConstant
  functions : List EncodedFunction
  entry : EncodedField
  deriving DecidableEq, Repr

structure EncodedProgramModule where
  name : EncodedField
  code : EncodedModule
  edges : List Edge
  bindings : List Binding
  exports : List Export
  deriving DecidableEq, Repr

inductive DecodeResult where
  | accepted (program : ProgramEnvelope)
  | rejected (error : DecodeError)
  deriving DecidableEq, Repr

structure Wire where
  magic : List Nat
  version : EncodedField
  entry : EncodedField
  modules : List EncodedProgramModule
  deriving DecidableEq, Repr

/-- Raw fields must have exactly one 7-bit group; extra groups are non-minimal. -/
def decodeField : EncodedField → Except DecodeError Nat
  | [value] =>
      if value < FieldLimit then .ok value else .error .malformedInteger
  | [] => .error .malformedInteger
  | _ => .error .nonCanonicalInteger

def encodeField (value : Nat) : EncodedField := [value]

def encodeOpcode : Op → EncodedField
  | .loadConst => encodeField 0
  | .binary => encodeField 3
  | .jump => encodeField 27
  | .suspend => encodeField 32
  | .halt => encodeField 35
  | .createCell => encodeField 36

def decodeOpcode (field : EncodedField) : Except DecodeError Op :=
  match decodeField field with
  | .error error => .error error
  | .ok 0 => .ok .loadConst
  | .ok 3 => .ok .binary
  | .ok 27 => .ok .jump
  | .ok 32 => .ok .suspend
  | .ok 35 => .ok .halt
  | .ok 36 => .ok .createCell
  | .ok _ => .error .invalidOpcode

def encodeInstr : Instr → EncodedInstr
  | .loadConst dst constant =>
      [encodeOpcode .loadConst, encodeField dst, encodeField constant]
  | .binary dst left right =>
      [encodeOpcode .binary, encodeField dst, encodeField 0, encodeField left, encodeField right]
  | .jump target => [encodeOpcode .jump, encodeField target]
  | .suspend dst src resumePc =>
      [encodeOpcode .suspend, encodeField dst, encodeField src, encodeField resumePc]
  | .halt => [encodeOpcode .halt]
  | .createCell dst => [encodeOpcode .createCell, encodeField dst]

def decodeInstr : EncodedInstr → Except DecodeError Instr
  | opcode :: operands =>
      match decodeOpcode opcode with
      | .error error => .error error
      | .ok .loadConst =>
          match operands with
          | [dst, constant] =>
              match decodeField dst, decodeField constant with
              | .ok dstValue, .ok constantValue => .ok (.loadConst dstValue constantValue)
              | .error error, _ => .error error
              | _, .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .binary =>
          match operands with
          | [dst, operator, left, right] =>
              match decodeField dst, decodeField operator, decodeField left, decodeField right with
              | .ok dstValue, .ok 0, .ok leftValue, .ok rightValue =>
                  .ok (.binary dstValue leftValue rightValue)
              | .ok _, .ok _, .ok _, .ok _ => .error .invalidBinaryOperator
              | .error error, _, _, _ => .error error
              | _, .error error, _, _ => .error error
              | _, _, .error error, _ => .error error
              | _, _, _, .error error => .error error
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
          | [dst, src, resumePc] =>
              match decodeField dst, decodeField src, decodeField resumePc with
              | .ok dstValue, .ok srcValue, .ok resumeValue =>
                  .ok (.suspend dstValue srcValue resumeValue)
              | .error error, _, _ => .error error
              | _, .error error, _ => .error error
              | _, _, .error error => .error error
          | _ => .error .malformedInstruction
      | .ok .halt =>
          match operands with
          | [] => .ok .halt
          | _ => .error .malformedInstruction
      | .ok .createCell =>
          match operands with
          | [dst] =>
              match decodeField dst with
              | .ok value => .ok (.createCell value)
              | .error error => .error error
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

def encodeFunction (function : Function) : EncodedFunction :=
  ⟨function.code.map encodeInstr, function.handlers.map encodeHandler⟩

def decodeFunction (encoded : EncodedFunction) : Except DecodeError Function :=
  match decodeInstructions encoded.instructions, decodeHandlers encoded.handlers with
  | .ok code, .ok handlers => .ok ⟨code, handlers⟩
  | .error error, _ => .error error
  | _, .error error => .error error

def decodeFunctions : List EncodedFunction → Except DecodeError (List Function)
  | [] => .ok []
  | encoded :: rest =>
      match decodeFunction encoded, decodeFunctions rest with
      | .ok function, .ok functions => .ok (function :: functions)
      | .error error, _ => .error error
      | _, .error error => .error error

/-- Encoded string payload: a length prefix followed by the exact UTF-16 code units. -/
def encodeString (units : List Nat) : List Nat := units.length :: units

def decodeString : List Nat → Except DecodeError (List Nat)
  | len :: rest =>
      if rest.length = len then .ok rest else .error .malformedConstant
  | [] => .error .malformedConstant

/-- BigInt text is encoded the same length-prefixed way as string units. -/
def encodeBigint (text : List Nat) : List Nat := text.length :: text

def decodeBigint : List Nat → Except DecodeError (List Nat)
  | len :: rest =>
      if rest.length = len then .ok rest else .error .malformedConstant
  | [] => .error .malformedConstant

/-- Int32 is encoded as sign and magnitude, preserving the two `Int` constructors. -/
def encodeInt32 (value : Int) : List Nat :=
  match value with
  | .ofNat n => [0, n]
  | .negSucc n => [1, n]

def decodeInt32 : List Nat → Except DecodeError Int
  | [0, n] => .ok (.ofNat n)
  | [1, n] => .ok (.negSucc n)
  | _ => .error .malformedConstant

/-- v4 constant-pool wire encoding: each constructor carries a production tag. -/
def encodeConstant : Constant → EncodedConstant
  | .numberBits bits => [0, bits]
  | .int32 value => 1 :: encodeInt32 value
  | .string units => 2 :: encodeString units
  | .boolean false => [3]
  | .boolean true => [4]
  | .null => [5]
  | .undefined => [6]
  | .bigint text => 7 :: encodeBigint text

def decodeConstant : EncodedConstant → Except DecodeError Constant
  | 0 :: bits :: [] => .ok (.numberBits bits)
  | 1 :: intEncoded =>
      match decodeInt32 intEncoded with
      | .ok value => .ok (.int32 value)
      | .error e => .error e
  | 2 :: stringEncoded =>
      match decodeString stringEncoded with
      | .ok units => .ok (.string units)
      | .error e => .error e
  | 3 :: [] => .ok (.boolean false)
  | 4 :: [] => .ok (.boolean true)
  | 5 :: [] => .ok .null
  | 6 :: [] => .ok .undefined
  | 7 :: bigintEncoded =>
      match decodeBigint bigintEncoded with
      | .ok text => .ok (.bigint text)
      | .error e => .error e
  | _ => .error .malformedConstant

def decodeConstants : List EncodedConstant → Except DecodeError (List Constant)
  | [] => .ok []
  | encoded :: rest =>
      match decodeConstant encoded, decodeConstants rest with
      | .ok constant, .ok constants => .ok (constant :: constants)
      | .error error, _ => .error error
      | _, .error error => .error error

theorem decodeInt32_encodeInt32 (value : Int) :
    decodeInt32 (encodeInt32 value) = .ok value := by
  cases value with
  | ofNat n => simp [encodeInt32, decodeInt32]
  | negSucc n => simp [encodeInt32, decodeInt32]

private theorem decodeString_cons (len : Nat) (rest : List Nat) (h : rest.length = len) :
    decodeString (len :: rest) = .ok rest := by
  simp only [decodeString]
  split
  · rfl
  · contradiction

private theorem decodeBigint_cons (len : Nat) (rest : List Nat) (h : rest.length = len) :
    decodeBigint (len :: rest) = .ok rest := by
  simp only [decodeBigint]
  split
  · rfl
  · contradiction

theorem decodeString_encodeString (units : List Nat) :
    decodeString (encodeString units) = .ok units := by
  simp only [encodeString]
  exact decodeString_cons units.length units (by rfl)

theorem decodeBigint_encodeBigint (text : List Nat) :
    decodeBigint (encodeBigint text) = .ok text := by
  simp only [encodeBigint]
  exact decodeBigint_cons text.length text (by rfl)

theorem decodeConstant_encodeConstant (constant : Constant) :
    decodeConstant (encodeConstant constant) = .ok constant := by
  cases constant with
  | numberBits bits => simp [encodeConstant, decodeConstant]
  | int32 value => simp [encodeConstant, decodeConstant, decodeInt32_encodeInt32 value]
  | string units => simp [encodeConstant, decodeConstant, decodeString_encodeString units]
  | boolean value =>
      cases value <;> simp [encodeConstant, decodeConstant]
  | null => simp [encodeConstant, decodeConstant]
  | undefined => simp [encodeConstant, decodeConstant]
  | bigint text => simp [encodeConstant, decodeConstant, decodeBigint_encodeBigint text]

/-- Ordinary (0x0061), NUL (0x0000), and surrogate (0xD800, 0xDC00, 0xDFFF)
code units round-trip through the constant codec exactly. -/
theorem decodeConstant_string_roundTrip_sample :
    decodeConstant (encodeConstant (Constant.string [97, 0, 55296, 56320, 57343])) =
      .ok (Constant.string [97, 0, 55296, 56320, 57343]) := by
  simp [encodeConstant, decodeConstant, decodeString_encodeString]

theorem decodeConstants_encodeConstants (constants : List Constant) :
    decodeConstants (constants.map encodeConstant) = .ok constants := by
  induction constants with
  | nil => rfl
  | cons c cs ih =>
      have hC := decodeConstant_encodeConstant c
      simp [decodeConstants, hC, ih]

def encodeModule (module : Module) : EncodedModule :=
  ⟨moduleMagicBytes, encodeField formatVersion, module.constants.map encodeConstant,
    module.functions.map encodeFunction, encodeField module.entry⟩

def decodeModule (encoded : EncodedModule) : Except DecodeError Module :=
  if encoded.magic ≠ moduleMagicBytes then .error .badMagic
  else
    match decodeField encoded.version with
    | .error error => .error error
    | .ok version =>
        if version ≠ formatVersion then .error .unsupportedVersion
        else
          match decodeConstants encoded.constants, decodeFunctions encoded.functions,
                decodeField encoded.entry with
          | .ok constants, .ok functions, .ok entry => .ok ⟨constants, functions, entry⟩
          | .error error, _, _ => .error error
          | _, .error error, _ => .error error
          | _, _, .error error => .error error

def encodeProgramModule (module : ProgramModule) : EncodedProgramModule :=
  ⟨encodeField module.name, encodeModule module.code, module.edges, module.bindings, module.exports⟩

def decodeProgramModule (encoded : EncodedProgramModule) : Except DecodeError ProgramModule :=
  match decodeField encoded.name, decodeModule encoded.code with
  | .ok name, .ok code => .ok ⟨name, code, encoded.edges, encoded.bindings, encoded.exports⟩
  | .error error, _ => .error error
  | _, .error error => .error error

def decodeProgramModules : List EncodedProgramModule → Except DecodeError (List ProgramModule)
  | [] => .ok []
  | encoded :: rest =>
      match decodeProgramModule encoded, decodeProgramModules rest with
      | .ok module, .ok modules => .ok (module :: modules)
      | .error error, _ => .error error
      | _, .error error => .error error


def encode (program : ProgramEnvelope) : Wire :=
  { magic := programMagicBytes
    version := encodeField formatVersion
    entry := encodeField program.entry
    modules := program.modules.map encodeProgramModule }

def decode (wire : Wire) : DecodeResult :=
  if wire.magic ≠ programMagicBytes then .rejected .badMagic
  else
    match decodeField wire.version with
    | .error error => .rejected error
    | .ok version =>
        if version ≠ formatVersion then .rejected .unsupportedVersion
        else
          match decodeField wire.entry, decodeProgramModules wire.modules with
          | .ok entry, .ok modules => .accepted ⟨modules, entry⟩
          | .error error, _ => .rejected error
          | _, .error error => .rejected error
theorem decode_rejects_bad_magic (wire : Wire) (badMagic : wire.magic ≠ programMagicBytes) :
    decode wire = .rejected .badMagic := by
  simp [decode, badMagic]

def instructionCanonical : Instr → Prop
  | .loadConst dst constant => dst < FieldLimit ∧ constant < FieldLimit
  | .binary dst left right => dst < FieldLimit ∧ left < FieldLimit ∧ right < FieldLimit
  | .jump target => target < FieldLimit
  | .suspend dst src resumePc =>
      dst < FieldLimit ∧ src < FieldLimit ∧ resumePc < FieldLimit
  | .halt => True
  | .createCell dst => dst < FieldLimit

def handlerCanonical (handler : Handler) : Prop :=
  handler.startPc < FieldLimit ∧ handler.endPc < FieldLimit ∧ handler.handlerPc < FieldLimit

def functionCanonical (function : Function) : Prop :=
  (∀ instruction ∈ function.code, instructionCanonical instruction) ∧
  ∀ handler ∈ function.handlers, handlerCanonical handler

abbrev functionBodyCanonical := functionCanonical

def moduleCanonical (module : Module) : Prop :=
  module.entry < FieldLimit ∧ ∀ function ∈ module.functions, functionCanonical function

def programModuleCanonical (module : ProgramModule) : Prop :=
  module.name < FieldLimit ∧ moduleCanonical module.code

def envelopeCanonical (program : ProgramEnvelope) : Prop :=
  program.entry < FieldLimit ∧
    ∀ module ∈ program.modules, programModuleCanonical module

def wireCanonical (wire : Wire) : Prop :=
  ∃ program, envelopeCanonical program ∧ wire = encode program

/-- Instruction boundaries are decoded structure, never a caller-supplied target list. -/
def instructionBoundaries (program : FunctionBody) : List Nat := List.range (program.code.length + 1)
def boundary (program : FunctionBody) (pc : Nat) : Prop := pc ∈ instructionBoundaries program

def controlTargets : List Instr → List Nat
  | [] => []
  | instruction :: rest =>
      match instruction.target? with
      | some target => target :: controlTargets rest
      | none => controlTargets rest

def targets (program : FunctionBody) : List Nat := controlTargets program.code

def backEdgeTargetsFrom (pc : Nat) : List Instr → List Nat
  | [] => []
  | .jump target :: rest =>
      if target ≤ pc then target :: backEdgeTargetsFrom (pc + 1) rest
      else backEdgeTargetsFrom (pc + 1) rest
  | _ :: rest => backEdgeTargetsFrom (pc + 1) rest

def backEdgeTargets (program : FunctionBody) : List Nat := backEdgeTargetsFrom 0 program.code

def suspendResumePcs : List Instr → List Nat
  | [] => []
  | .suspend _ _ resumePc :: rest => resumePc :: suspendResumePcs rest
  | _ :: rest => suspendResumePcs rest

def entryCandidates (program : FunctionBody) : List Nat :=
  0 :: (backEdgeTargets program ++ suspendResumePcs program.code)

/-- Filtering the ordered instruction-boundary list gives sorted, duplicate-free entry ordinals. -/
def entryPoints (program : FunctionBody) : List Nat :=
  (List.range program.code.length).filter fun pc => decide (pc ∈ entryCandidates program)

def nextPc (pc : Nat) : Instr → Option Nat
  | .loadConst _ _ => some (pc + 1)
  | .binary _ _ _ => some (pc + 1)
  | .jump target => some target
  | .suspend _ _ resumePc => some resumePc
  | .halt => none
  | .createCell _ => some (pc + 1)

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
  | loadConst dst constant =>
      rcases h with ⟨dstCanonical, constantCanonical⟩
      have dstLt : dst < 128 := by simpa [FieldLimit] using dstCanonical
      have constantLt : constant < 128 := by simpa [FieldLimit] using constantCanonical
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit, dstLt, constantLt]
  | binary dst left right =>
      rcases h with ⟨dstCanonical, leftCanonical, rightCanonical⟩
      have dstLt : dst < 128 := by simpa [FieldLimit] using dstCanonical
      have leftLt : left < 128 := by simpa [FieldLimit] using leftCanonical
      have rightLt : right < 128 := by simpa [FieldLimit] using rightCanonical
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit, dstLt, leftLt, rightLt]
  | jump target =>
      simp only [instructionCanonical] at h
      have targetLt : target < 128 := by simpa [FieldLimit] using h
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit, targetLt]
  | suspend dst src resumePc =>
      rcases h with ⟨dstCanonical, srcCanonical, resumeCanonical⟩
      have dstLt : dst < 128 := by simpa [FieldLimit] using dstCanonical
      have srcLt : src < 128 := by simpa [FieldLimit] using srcCanonical
      have resumeLt : resumePc < 128 := by simpa [FieldLimit] using resumeCanonical
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit, dstLt, srcLt, resumeLt]
  | halt =>
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit]
  | createCell dst =>
      simp only [instructionCanonical] at h
      have dstLt : dst < 128 := by simpa [FieldLimit] using h
      simp [encodeInstr, decodeInstr, decodeOpcode, encodeOpcode, decodeField, encodeField,
        FieldLimit, dstLt]

theorem decodeHandler_encodeHandler (handler : Handler) (h : handlerCanonical handler) :
    decodeHandler (encodeHandler handler) = .ok handler := by
  rcases handler with ⟨startPc, endPc, handlerPc⟩
  rcases h with ⟨startCanonical, endCanonical, handlerCanonical⟩
  have startLt : startPc < 128 := by simpa [FieldLimit] using startCanonical
  have endLt : endPc < 128 := by simpa [FieldLimit] using endCanonical
  have handlerLt : handlerPc < 128 := by simpa [FieldLimit] using handlerCanonical
  simp [encodeHandler, decodeHandler, decodeField, encodeField, FieldLimit,
    startLt, endLt, handlerLt]

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

theorem decodeFunction_encodeFunction (function : Function) (h : functionCanonical function) :
    decodeFunction (encodeFunction function) = .ok function := by
  rcases h with ⟨codeCanonical, handlersCanonical⟩
  simp [decodeFunction, encodeFunction,
    decodeInstructions_encodeInstructions function.code codeCanonical,
    decodeHandlers_encodeHandlers function.handlers handlersCanonical]

theorem decodeFunctions_encodeFunctions (functions : List Function)
    (h : ∀ function ∈ functions, functionCanonical function) :
    decodeFunctions (functions.map encodeFunction) = .ok functions := by
  induction functions with
  | nil => rfl
  | cons function rest ih =>
      have hFunction : functionCanonical function := h function (by simp)
      have hRest : ∀ remaining ∈ rest, functionCanonical remaining := by
        intro remaining hRemaining
        exact h remaining (by simp [hRemaining])
      simp [decodeFunctions, decodeFunction_encodeFunction function hFunction, ih hRest]

theorem decodeModule_encodeModule (module : Module) (h : moduleCanonical module) :
    decodeModule (encodeModule module) = .ok module := by
  rcases h with ⟨entryCanonical, functionsCanonical⟩
  have versionBound : formatVersion < FieldLimit := by decide
  simp [decodeModule, encodeModule, decodeField_encodeField formatVersion versionBound,
    decodeField_encodeField module.entry entryCanonical,
    decodeConstants_encodeConstants module.constants,
    decodeFunctions_encodeFunctions module.functions functionsCanonical]

theorem decodeProgramModule_encodeProgramModule
    (module : ProgramModule) (h : programModuleCanonical module) :
    decodeProgramModule (encodeProgramModule module) = .ok module := by
  rcases h with ⟨nameCanonical, codeCanonical⟩
  simp [decodeProgramModule, encodeProgramModule,
    decodeField_encodeField module.name nameCanonical,
    decodeModule_encodeModule module.code codeCanonical]

theorem decodeProgramModules_encodeProgramModules (modules : List ProgramModule)
    (h : ∀ module ∈ modules, programModuleCanonical module) :
    decodeProgramModules (modules.map encodeProgramModule) = .ok modules := by
  induction modules with
  | nil => rfl
  | cons module rest ih =>
      have hModule : programModuleCanonical module := h module (by simp)
      have hRest : ∀ remaining ∈ rest, programModuleCanonical remaining := by
        intro remaining hRemaining
        exact h remaining (by simp [hRemaining])
      simp [decodeProgramModules, decodeProgramModule_encodeProgramModule module hModule, ih hRest]

end Bamti.Bytecode
