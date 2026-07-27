import Bamti.Bytecode.Bridge

namespace Bamti.Compiler

/-- The closed catalog of semantic operations covered by the source-to-bytecode relation. -/
inductive SemanticRule where
  | literal
  | «variable»
  | property_get
  | property_set
  | call
  | construct
  | conditional
  | sequence
  | «loop»
  | «throw»
  | try_catch_finally
  | iterator
  | promise
  | «async»
  | binding
  | module_link
  | module_evaluate
  | dynamic_import
  deriving DecidableEq, Repr

/-- Source syntax is deliberately distinct from bytecode instructions. -/
inductive SourceForm where
  | literalExpr
  | variableExpr
  | propertyGetExpr
  | propertySetExpr
  | callExpr
  | constructExpr
  | conditionalExpr
  | sequenceExpr
  | loopExpr
  | throwExpr
  | tryCatchFinallyExpr
  | iteratorExpr
  | promiseExpr
  | asyncExpr
  | bindingExpr
  | moduleLinkExpr
  | moduleEvaluateExpr
  | dynamicImportExpr
  deriving DecidableEq, Repr

/-- Observable completions emitted by the source and the interpreter. -/
inductive Observable where
  | normal (rule : SemanticRule)
  | thrown (rule : SemanticRule)
  | suspended (rule : SemanticRule)
  | moduleEffect (rule : SemanticRule)
  deriving DecidableEq, Repr

/-- The instruction domain has one opcode per semantic operation plus report and malformed words. -/
inductive Instruction where
  | loadLiteral
  | loadVariable
  | getProperty
  | setProperty
  | invoke
  | allocateObject
  | branchIf
  | evaluateSequence
  | jumpBack
  | raiseException
  | installHandler
  | iteratorNext
  | promiseThen
  | suspendAsync
  | bindName
  | linkModule
  | evaluateModule
  | importModule
  | report (effect : Observable)
  | malformed
  deriving DecidableEq, Repr

structure Source where
  forms : List SourceForm
  deriving DecidableEq, Repr

structure Machine where
  code : List Instruction
  deriving DecidableEq, Repr

structure SourceState where
  program : Source
  trace : List Observable
  deriving DecidableEq, Repr

structure BytecodeState where
  machine : Machine
  trace : List Observable
  deriving DecidableEq, Repr

def sourceRule : SourceForm → SemanticRule
  | .literalExpr => .literal
  | .variableExpr => .«variable»
  | .propertyGetExpr => .property_get
  | .propertySetExpr => .property_set
  | .callExpr => .call
  | .constructExpr => .construct
  | .conditionalExpr => .conditional
  | .sequenceExpr => .sequence
  | .loopExpr => .«loop»
  | .throwExpr => .«throw»
  | .tryCatchFinallyExpr => .try_catch_finally
  | .iteratorExpr => .iterator
  | .promiseExpr => .promise
  | .asyncExpr => .«async»
  | .bindingExpr => .binding
  | .moduleLinkExpr => .module_link
  | .moduleEvaluateExpr => .module_evaluate
  | .dynamicImportExpr => .dynamic_import

/-- Effects retain their semantic category instead of collapsing rules to integer labels. -/
def ruleObservable : SemanticRule → Observable
  | .«throw» => .thrown .«throw»
  | .iterator => .suspended .iterator
  | .promise => .suspended .promise
  | .«async» => .suspended .«async»
  | .module_link => .moduleEffect .module_link
  | .module_evaluate => .moduleEffect .module_evaluate
  | .dynamic_import => .moduleEffect .dynamic_import
  | rule => .normal rule

def opcodeFor : SemanticRule → Instruction
  | .literal => .loadLiteral
  | .«variable» => .loadVariable
  | .property_get => .getProperty
  | .property_set => .setProperty
  | .call => .invoke
  | .construct => .allocateObject
  | .conditional => .branchIf
  | .sequence => .evaluateSequence
  | .«loop» => .jumpBack
  | .«throw» => .raiseException
  | .try_catch_finally => .installHandler
  | .iterator => .iteratorNext
  | .promise => .promiseThen
  | .«async» => .suspendAsync
  | .binding => .bindName
  | .module_link => .linkModule
  | .module_evaluate => .evaluateModule
  | .dynamic_import => .importModule

def instructionRule? : Instruction → Option SemanticRule
  | .loadLiteral => some .literal
  | .loadVariable => some .«variable»
  | .getProperty => some .property_get
  | .setProperty => some .property_set
  | .invoke => some .call
  | .allocateObject => some .construct
  | .branchIf => some .conditional
  | .evaluateSequence => some .sequence
  | .jumpBack => some .«loop»
  | .raiseException => some .«throw»
  | .installHandler => some .try_catch_finally
  | .iteratorNext => some .iterator
  | .promiseThen => some .promise
  | .suspendAsync => some .«async»
  | .bindName => some .binding
  | .linkModule => some .module_link
  | .evaluateModule => some .module_evaluate
  | .importModule => some .dynamic_import
  | .report _ => none
  | .malformed => none

def instructionObservable : Instruction → Option Observable
  | .report effect => some effect
  | _ => none

@[simp] theorem opcodeFor_silent (rule : SemanticRule) :
    instructionObservable (opcodeFor rule) = none := by
  cases rule <;> rfl

@[simp] theorem report_visible (effect : Observable) :
    instructionObservable (.report effect) = some effect := rfl

/-- Lowering inserts an internal opcode followed by a visible completion report. -/
def lowerForm (form : SourceForm) : List Instruction :=
  [opcodeFor (sourceRule form), .report (ruleObservable (sourceRule form))]

def lowerProgram : List SourceForm → List Instruction
  | [] => []
  | form :: rest => lowerForm form ++ lowerProgram rest

def compile (source : Source) : Machine :=
  { code := lowerProgram source.forms }

/-- The bounded model includes a malformed bytecode word that the verifier rejects. -/
def instructionAccepted : Instruction → Bool
  | .malformed => false
  | _ => true

def codeAccepted : List Instruction → Bool
  | [] => true
  | instruction :: rest => instructionAccepted instruction && codeAccepted rest

theorem malformed_instruction_rejected : codeAccepted [.malformed] = false := by
  rfl

@[simp] theorem opcodeFor_accepted (rule : SemanticRule) :
    instructionAccepted (opcodeFor rule) = true := by
  cases rule <;> rfl

@[simp] theorem report_accepted (effect : Observable) :
    instructionAccepted (.report effect) = true := rfl

/-- A verified stream consists only of correctly paired opcode/report translations. -/
inductive VerifiedCode : List Instruction → Prop where
  | nil : VerifiedCode []
  | pair (rule : SemanticRule) {rest : List Instruction} :
      VerifiedCode rest →
      VerifiedCode (opcodeFor rule :: .report (ruleObservable rule) :: rest)

theorem lowerProgram_verified (forms : List SourceForm) :
    VerifiedCode (lowerProgram forms) := by
  induction forms with
  | nil => exact .nil
  | cons form rest ih =>
      simpa [lowerProgram, lowerForm] using
        (VerifiedCode.pair (sourceRule form) ih)

theorem verifiedCode_accepted {code : List Instruction} (h : VerifiedCode code) :
    codeAccepted code = true := by
  induction h with
  | nil => rfl
  | pair rule _ ih => simp [codeAccepted, ih]

theorem malformed_not_verified : ¬ VerifiedCode [.malformed] := by
  intro h
  have accepted := verifiedCode_accepted h
  simp [codeAccepted, instructionAccepted] at accepted

/-- Source reduction removes one source form and exposes its semantic effect. -/
def srcStep (program : Source) : Source :=
  match program.forms with
  | [] => program
  | _ :: rest => { forms := rest }

def sourceStep (state : SourceState) : SourceState :=
  match state.program.forms with
  | [] => state
  | form :: _ =>
      { program := srcStep state.program
        trace := state.trace ++ [ruleObservable (sourceRule form)] }

/-- Bytecode reduction consumes one instruction from a remaining instruction stream. -/
def currentInstruction (machine : Machine) : Option Instruction :=
  match machine.code with
  | [] => none
  | instruction :: _ => some instruction

def bcStep (machine : Machine) : Machine :=
  match machine.code with
  | [] => machine
  | _ :: rest => { code := rest }

def bytecodeStep (state : BytecodeState) : BytecodeState :=
  match currentInstruction state.machine with
  | none => state
  | some instruction =>
      match instructionObservable instruction with
      | none => { machine := bcStep state.machine, trace := state.trace }
      | some effect =>
          { machine := bcStep state.machine, trace := state.trace ++ [effect] }

/-- Ordinary executions count individual source and bytecode transitions. -/
def sourceRun : Nat → SourceState → SourceState
  | 0, state => state
  | Nat.succ steps, state => sourceRun steps (sourceStep state)

def bytecodeRun : Nat → BytecodeState → BytecodeState
  | 0, state => state
  | Nat.succ steps, state => bytecodeRun steps (bytecodeStep state)

/-- One source transition is simulated by two bytecode transitions. -/
def bytecodeWeakRun : Nat → BytecodeState → BytecodeState
  | 0, state => state
  | Nat.succ steps, state => bytecodeWeakRun steps (bytecodeRun 2 state)

/-- State traces make multi-step executions inspectable as finite lists. -/
def sourceExecutionTrace : Nat → SourceState → List SourceState
  | 0, state => [state]
  | Nat.succ steps, state => state :: sourceExecutionTrace steps (sourceStep state)

def bytecodeExecutionTrace : Nat → BytecodeState → List BytecodeState
  | 0, state => [state]
  | Nat.succ steps, state => state :: bytecodeExecutionTrace steps (bytecodeStep state)

def sourceObservations (steps : Nat) (state : SourceState) : List Observable :=
  (sourceRun steps state).trace

def bytecodeWeakObservations (steps : Nat) (state : BytecodeState) : List Observable :=
  (bytecodeWeakRun steps state).trace

def lastObservable : List Observable → Option Observable
  | [] => none
  | effect :: rest =>
      match lastObservable rest with
      | none => some effect
      | some last => some last

def sourceOutcome (state : SourceState) : Option Observable :=
  match state.program.forms with
  | [] => lastObservable state.trace
  | _ :: _ => none

def bytecodeOutcome (state : BytecodeState) : Option Observable :=
  match currentInstruction state.machine with
  | none => lastObservable state.trace
  | some _ => none

def observation (state : BytecodeState) : Option Observable :=
  bytecodeOutcome state

def initialBytecodeState (machine : Machine) (history : List Observable) : BytecodeState :=
  { machine := machine, trace := history }

/-- Compilation builds a target instruction stream and a fresh target execution state. -/
def compileState (state : SourceState) : BytecodeState :=
  initialBytecodeState (compile state.program) state.trace

def emptySourceState (history : List Observable) : SourceState :=
  { program := { forms := [] }, trace := history }

def singleFormState (form : SourceForm) (history : List Observable) : SourceState :=
  { program := { forms := [form] }, trace := history }

/-- Related states have the same visible history and target code for the remaining source forms. -/
def related (sourceState : SourceState) (bytecodeState : BytecodeState) : Prop :=
  bytecodeState.machine.code = lowerProgram sourceState.program.forms ∧
    bytecodeState.trace = sourceState.trace

theorem compileState_related (state : SourceState) : related state (compileState state) := by
  constructor <;> simp [compileState, initialBytecodeState, compile]

theorem source_step_deterministic (s t u : SourceState)
    (h₁ : sourceStep s = t) (h₂ : sourceStep s = u) : t = u := by
  exact h₁.symm.trans h₂

theorem bytecode_step_deterministic (s t u : BytecodeState)
    (h₁ : bytecodeStep s = t) (h₂ : bytecodeStep s = u) : t = u := by
  exact h₁.symm.trans h₂

theorem src_step_deterministic (s t u : Source)
    (h₁ : srcStep s = t) (h₂ : srcStep s = u) : t = u := by
  exact h₁.symm.trans h₂

theorem bc_step_deterministic (s t u : Machine)
    (h₁ : bcStep s = t) (h₂ : bcStep s = u) : t = u := by
  exact h₁.symm.trans h₂

/-- A related target takes two concrete instructions for each source transition. -/
theorem weak_step_simulation (sourceState : SourceState) (bytecodeState : BytecodeState)
    (h : related sourceState bytecodeState) :
    related (sourceStep sourceState) (bytecodeRun 2 bytecodeState) := by
  rcases sourceState with ⟨⟨forms⟩, trace⟩
  rcases bytecodeState with ⟨⟨code⟩, byteTrace⟩
  rcases h with ⟨hcode, htrace⟩
  cases forms with
  | nil =>
      have codeEmpty : code = [] := by
        simpa [lowerProgram] using hcode
      have traceEq : byteTrace = trace := by
        simpa using htrace
      simp [sourceStep, bytecodeRun, bytecodeStep,
        currentInstruction, related, lowerProgram, codeEmpty, traceEq]
  | cons form rest =>
      have codeLowered : code = lowerForm form ++ lowerProgram rest := by
        simpa [lowerProgram] using hcode
      have traceEq : byteTrace = trace := by
        simpa using htrace
      cases form <;>
        simp [sourceStep, srcStep, bytecodeRun, bytecodeStep, bcStep,
          currentInstruction, instructionObservable, related,
          lowerForm, sourceRule, ruleObservable, opcodeFor, codeLowered, traceEq]

/-- The cataloged one-step theorem starts from a real compiled target, not a copied state. -/
theorem compile_state_step_commutes (state : SourceState) :
    related (sourceStep state) (bytecodeRun 2 (compileState state)) :=
  weak_step_simulation state (compileState state) (compileState_related state)

theorem related_outcomes (sourceState : SourceState) (bytecodeState : BytecodeState)
    (h : related sourceState bytecodeState) :
    sourceOutcome sourceState = bytecodeOutcome bytecodeState := by
  rcases sourceState with ⟨⟨forms⟩, trace⟩
  rcases bytecodeState with ⟨⟨code⟩, byteTrace⟩
  rcases h with ⟨hcode, htrace⟩
  cases forms with
  | nil =>
      have codeEmpty : code = [] := by
        simpa [lowerProgram] using hcode
      have traceEq : byteTrace = trace := by
        simpa using htrace
      simp [sourceOutcome, bytecodeOutcome, currentInstruction,
        codeEmpty, traceEq]
  | cons form rest =>
      have codeLowered : code = lowerForm form ++ lowerProgram rest := by
        simpa [lowerProgram] using hcode
      have traceEq : byteTrace = trace := by
        simpa using htrace
      simp [sourceOutcome, bytecodeOutcome, currentInstruction,
        lowerForm, codeLowered]

end Bamti.Compiler
