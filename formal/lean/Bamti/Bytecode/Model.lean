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

/-- One production function body. `Program` remains an alias for execution proofs. -/
structure Function where
  code : List Instr
  handlers : List Handler
  deriving DecidableEq, Repr

abbrev Program := Function

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
  constants : List Constant
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

def encodeModule (module : Module) : EncodedModule :=
  ⟨moduleMagicBytes, encodeField formatVersion, module.constants,
    module.functions.map encodeFunction, encodeField module.entry⟩

def decodeModule (encoded : EncodedModule) : Except DecodeError Module :=
  if encoded.magic ≠ moduleMagicBytes then .error .badMagic
  else
    match decodeField encoded.version with
    | .error error => .error error
    | .ok version =>
        if version ≠ formatVersion then .error .unsupportedVersion
        else
          match decodeFunctions encoded.functions, decodeField encoded.entry with
          | .ok functions, .ok entry => .ok ⟨encoded.constants, functions, entry⟩
          | .error error, _ => .error error
          | _, .error error => .error error

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

abbrev programCanonical := functionCanonical

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
  | .suspend _ _ resumePc :: rest => resumePc :: suspendResumePcs rest
  | _ :: rest => suspendResumePcs rest

def entryCandidates (program : Program) : List Nat :=
  0 :: (backEdgeTargets program ++ suspendResumePcs program.code)

/-- Filtering the ordered instruction-boundary list gives sorted, duplicate-free entry ordinals. -/
def entryPoints (program : Program) : List Nat :=
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
