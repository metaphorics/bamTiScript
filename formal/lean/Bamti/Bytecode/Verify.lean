import Bamti.Bytecode.Model

namespace Bamti.Bytecode

/-- This bounded model reserves the one-past-last boundary inside one 7-bit address space. -/
def withinModelBound (program : Program) : Prop := program.code.length < FieldLimit

def instructionAt (program : Program) (pc : Nat) : Option Instr := program.code[pc]?

def cfgTargetsValid (program : Program) : Prop :=
  ∀ target ∈ targets program,
    boundary program target ∧ ∃ instruction, instructionAt program target = some instruction

def handlerBounds (program : Program) (handler : Handler) : Prop :=
  handler.startPc < handler.endPc ∧
  handler.endPc ≤ program.code.length ∧
  boundary program handler.startPc ∧
  boundary program handler.endPc ∧
  boundary program handler.handlerPc ∧
  ∃ instruction, instructionAt program handler.handlerPc = some instruction

def intervalsNested (left right : Handler) : Prop :=
  left.endPc ≤ right.startPc ∨
  right.endPc ≤ left.startPc ∨
  (left.startPc ≤ right.startPc ∧ right.endPc ≤ left.endPc) ∨
  (right.startPc ≤ left.startPc ∧ left.endPc ≤ right.endPc)

def handlersNested (program : Program) : Prop :=
  (∀ handler ∈ program.handlers, handlerBounds program handler) ∧
  ∀ left ∈ program.handlers, ∀ right ∈ program.handlers, intervalsNested left right

def factAt : List (List Nat) → Nat → List Nat
  | [], _ => []
  | facts :: _, 0 => facts
  | _ :: rest, pc + 1 => factAt rest pc

/-- A certificate is a syntactic forward dataflow witness, not a semantic reachability premise. -/
structure Certificate (program : Program) where
  facts : List (List Nat)
  factCount : facts.length = program.code.length
  entryFactsEmpty : factAt facts 0 = []
  readsCovered : ∀ pc (instruction : Instr),
    instructionAt program pc = some instruction →
    ∀ register ∈ Instr.reads instruction, register ∈ factAt facts pc
  transfers : ∀ pc (instruction : Instr) next,
    instructionAt program pc = some instruction →
    nextPc pc instruction = some next →
    ∀ register ∈ factAt facts next,
      register ∈ Instr.writes instruction ∨ register ∈ factAt facts pc
  successors : ∀ pc (instruction : Instr) next,
    instructionAt program pc = some instruction →
    nextPc pc instruction = some next →
    ∃ nextInstruction,
      instructionAt program next = some nextInstruction ∧ boundary program next

def hasEntry (program : Program) : Prop :=
  ∃ instruction, instructionAt program 0 = some instruction ∧ boundary program 0

structure Verification (program : Program) where
  canonical : programCanonical program
  bounded : withinModelBound program
  entry : hasEntry program
  cfg : cfgTargetsValid program
  handlers : handlersNested program
  certificate : Certificate program

def verifies (program : Program) : Prop := Nonempty (Verification program)

inductive VerifyResult where
  | accepted
  | rejected
  deriving DecidableEq, Repr

/-- The verifier's specification-level decision is defined only from syntax and certificates. -/
noncomputable def verify (program : Program) : VerifyResult := by
  classical
  exact if verifies program then .accepted else .rejected

/-- The model is pinned to the production v4 envelope, module blob, and `CreateCell` tag. -/
theorem format_v4_createCell_tag :
    formatVersion = 4 ∧
      programMagicBytes = [66, 77, 84, 80, 67, 0, 0, 1] ∧
      moduleMagicBytes = [66, 77, 84, 66, 67, 0, 0, 1] ∧
      encodeOpcode .createCell = encodeField 36 := by
  decide

theorem decode_total (wire : Wire) :
    (∃ program, decode wire = .accepted program) ∨
      ∃ error, decode wire = .rejected error := by
  cases decode wire with
  | accepted program => exact Or.inl ⟨program, rfl⟩
  | rejected error => exact Or.inr ⟨error, rfl⟩

theorem decode_encode_canonical (program : ProgramEnvelope) (h : envelopeCanonical program) :
    decode (encode program) = .accepted program := by
  rcases h with ⟨entryCanonical, modulesCanonical⟩
  have versionBound : formatVersion < FieldLimit := by decide
  simp [decode, encode, decodeField_encodeField formatVersion versionBound,
    decodeField_encodeField program.entry entryCanonical,
    decodeProgramModules_encodeProgramModules program.modules modulesCanonical]

theorem encode_decode_identity (wire : Wire) (program : ProgramEnvelope)
    (hdecode : decode wire = .accepted program) (hcanonical : wireCanonical wire) :
    encode program = wire := by
  rcases hcanonical with ⟨canonicalProgram, canonicalProgramCanonical, hwire⟩
  subst wire
  have sameProgram : DecodeResult.accepted program = DecodeResult.accepted canonicalProgram := by
    calc
      DecodeResult.accepted program = decode (encode canonicalProgram) := hdecode.symm
      _ = DecodeResult.accepted canonicalProgram :=
        decode_encode_canonical canonicalProgram canonicalProgramCanonical
  have programEq : program = canonicalProgram := by
    cases sameProgram
    rfl
  simp [programEq]

theorem cfg_targets_are_boundaries (program : Program) (h : verifies program) :
    ∀ target ∈ targets program,
      boundary program target ∧ ∃ instruction, instructionAt program target = some instruction := by
  rcases h with ⟨verification⟩
  exact verification.cfg


inductive MachineMode where
  | running
  | halted
  deriving DecidableEq, Repr

structure MachineState where
  pc : Nat
  initialized : List Nat
  mode : MachineMode
  deriving DecidableEq, Repr

def initialState : MachineState := ⟨0, [], .running⟩

/-- Small-step execution only inspects bytecode and machine state; it never calls `verifies`. -/
inductive Executes (program : Program) : MachineState → MachineState → Prop where
  | advance (state : MachineState) (instruction : Instr) (next : Nat)
      (running : state.mode = .running)
      (fetch : instructionAt program state.pc = some instruction)
      (readsReady : ∀ register ∈ Instr.reads instruction, register ∈ state.initialized)
      (nextIs : nextPc state.pc instruction = some next) :
      Executes program state
        ⟨next, Instr.writes instruction ++ state.initialized, .running⟩
  | halt (state : MachineState) (instruction : Instr)
      (running : state.mode = .running)
      (fetch : instructionAt program state.pc = some instruction)
      (readsReady : ∀ register ∈ Instr.reads instruction, register ∈ state.initialized)
      (stops : nextPc state.pc instruction = none) :
      Executes program state ⟨state.pc, state.initialized, .halted⟩

inductive Reaches (program : Program) : MachineState → Prop where
  | initial : Reaches program initialState
  | step {state next : MachineState} : Reaches program state → Executes program state next → Reaches program next
def handlerDispatchSafe (program : Program) : Prop :=
  ∀ state, Reaches program state → ∀ handler ∈ program.handlers,
    handler.startPc ≤ state.pc → state.pc < handler.endPc →
      boundary program handler.handlerPc ∧
        ∃ instruction, instructionAt program handler.handlerPc = some instruction

def handlerSafety (program : Program) : Prop :=
  handlersNested program ∧ handlerDispatchSafe program

theorem handlers_well_nested (program : Program) (h : verifies program) : handlerSafety program := by
  rcases h with ⟨verification⟩
  refine ⟨verification.handlers, ?_⟩
  intro state _ handler handlerMember _ _
  rcases verification.handlers.1 handler handlerMember with
    ⟨_, _, _, _, handlerBoundary, handlerInstruction⟩
  exact ⟨handlerBoundary, handlerInstruction⟩

def factsHeld (certificate : Certificate program) (state : MachineState) : Prop :=
  ∀ register ∈ factAt certificate.facts state.pc, register ∈ state.initialized

def machineInvariant (program : Program) (certificate : Certificate program)
    (state : MachineState) : Prop :=
  state.mode = .halted ∨
    ∃ instruction,
      instructionAt program state.pc = some instruction ∧
      boundary program state.pc ∧ factsHeld certificate state

def reachableInvariant (program : Program) : Prop :=
  ∃ certificate : Certificate program,
    ∀ state, Reaches program state → machineInvariant program certificate state

def reachableReadSafe (program : Program) : Prop :=
  ∃ _ : Certificate program,
    ∀ state, Reaches program state → ∀ instruction,
      instructionAt program state.pc = some instruction → state.mode = .running →
        ∀ register ∈ Instr.reads instruction, register ∈ state.initialized

def executionSafe (program : Program) : Prop :=
  programCanonical program ∧
  withinModelBound program ∧
  cfgTargetsValid program ∧
  handlerDispatchSafe program ∧
  ∃ certificate : Certificate program,
    ∀ state, Reaches program state →
      machineInvariant program certificate state ∧
        (state.mode = .halted ∨ ∃ next, Executes program state next)

private theorem initial_machine_invariant (verification : Verification program) :
    machineInvariant program verification.certificate initialState := by
  rcases verification.entry with ⟨instruction, fetch, entryBoundary⟩
  refine Or.inr ⟨instruction, fetch, entryBoundary, ?_⟩
  change ∀ register ∈ factAt verification.certificate.facts 0, register ∈ ([] : List Nat)
  intro register registerFact
  rw [verification.certificate.entryFactsEmpty] at registerFact
  exact registerFact

private theorem machineInvariant_step (verification : Verification program)
    {state next : MachineState}
    (invariant : machineInvariant program verification.certificate state)
    (step : Executes program state next) :
    machineInvariant program verification.certificate next := by
  cases step with
  | advance instruction next running fetch readsReady nextIs =>
      rcases invariant with halted | ⟨current, currentFetch, currentBoundary, currentFacts⟩
      · cases running.symm.trans halted
      · rcases verification.certificate.successors state.pc instruction next fetch nextIs with
          ⟨nextInstruction, nextFetch, nextBoundary⟩
        refine Or.inr ⟨nextInstruction, nextFetch, nextBoundary, ?_⟩
        change ∀ register ∈ factAt verification.certificate.facts next,
          register ∈ Instr.writes instruction ++ state.initialized
        intro register registerFact
        rcases verification.certificate.transfers state.pc instruction next fetch nextIs register registerFact with
          written | previous
        · exact List.mem_append.mpr (Or.inl written)
        · exact List.mem_append.mpr (Or.inr (currentFacts register previous))
  | halt instruction running fetch readsReady stops =>
      exact Or.inl rfl

private theorem reachable_machine_invariant (verification : Verification program) :
    ∀ state, Reaches program state → machineInvariant program verification.certificate state := by
  intro state reaches
  induction reaches with
  | initial => exact initial_machine_invariant verification
  | @step state next reaches step inductionHypothesis =>
      exact machineInvariant_step verification inductionHypothesis step

private theorem progress (verification : Verification program) (state : MachineState)
    (invariant : machineInvariant program verification.certificate state) :
    state.mode = .halted ∨ ∃ next, Executes program state next := by
  rcases invariant with halted | ⟨instruction, fetch, currentBoundary, currentFacts⟩
  · exact Or.inl halted
  · cases mode : state.mode with
    | halted => exact Or.inl rfl
    | running =>
        right
        have readsReady : ∀ register ∈ Instr.reads instruction, register ∈ state.initialized := by
          intro register read
          exact currentFacts register
            (verification.certificate.readsCovered state.pc instruction fetch register read)
        cases nextIs : nextPc state.pc instruction with
        | none =>
            exact ⟨⟨state.pc, state.initialized, .halted⟩,
              Executes.halt state instruction mode fetch readsReady nextIs⟩
        | some next =>
            exact ⟨⟨next, Instr.writes instruction ++ state.initialized, .running⟩,
              Executes.advance state instruction next mode fetch readsReady nextIs⟩

theorem definite_init_sound (program : Program) (h : verifies program) : reachableInvariant program := by
  rcases h with ⟨verification⟩
  exact ⟨verification.certificate, reachable_machine_invariant verification⟩

theorem entry_points_complete (program : Program) (target : Nat)
    (entryTarget : target ∈ backEdgeTargets program ∨ target ∈ suspendResumePcs program.code)
    (inCode : target < program.code.length) : target ∈ entryPoints program := by
  have candidate : target ∈ entryCandidates program := by
    simp only [entryCandidates, List.mem_cons, List.mem_append]
    exact Or.inr entryTarget
  exact List.mem_filter.mpr ⟨List.mem_range.mpr inCode, by simp [candidate]⟩

def ordinal : Nat → List Nat → Nat
  | _, [] => 0
  | value, current :: rest => if value = current then 0 else ordinal value rest + 1

private theorem ordinal_injective_on (values : List Nat) (left right : Nat)
    (leftMember : left ∈ values) (rightMember : right ∈ values)
    (sameOrdinal : ordinal left values = ordinal right values) : left = right := by
  induction values with
  | nil => simp at leftMember
  | cons current rest inductionHypothesis =>
      simp only [List.mem_cons] at leftMember rightMember
      by_cases leftCurrent : left = current
      · subst left
        by_cases rightCurrent : right = current
        · exact rightCurrent.symm
        · simp [ordinal, rightCurrent] at sameOrdinal
      · by_cases rightCurrent : right = current
        · subst right
          simp [ordinal, leftCurrent] at sameOrdinal
        · have tailOrdinal : ordinal left rest = ordinal right rest := by
            simpa [ordinal, leftCurrent, rightCurrent] using sameOrdinal
          exact inductionHypothesis (leftMember.resolve_left leftCurrent)
            (rightMember.resolve_left rightCurrent) tailOrdinal

theorem entry_ordinal_injective (program : Program) (left right : Nat)
    (leftMember : left ∈ entryPoints program) (rightMember : right ∈ entryPoints program)
    (sameOrdinal : ordinal left (entryPoints program) = ordinal right (entryPoints program)) : left = right :=
  ordinal_injective_on (entryPoints program) left right leftMember rightMember sameOrdinal

theorem verify_ok_iff (program : Program) : verify program = .accepted ↔ verifies program := by
  classical
  unfold verify
  split <;> simp_all

theorem verify_sound (program : Program) (h : verifies program) : executionSafe program := by
  rcases h with ⟨verification⟩
  refine ⟨verification.canonical, verification.bounded, verification.cfg, ?_,
    verification.certificate, ?_⟩
  · exact (handlers_well_nested program ⟨verification⟩).2
  · intro state reaches
    have invariant := reachable_machine_invariant verification state reaches
    exact ⟨invariant, progress verification state invariant⟩

theorem verifier_never_skips_invariant (program : Program) (h : verifies program) :
    executionSafe program ∧ reachableReadSafe program := by
  constructor
  · exact verify_sound program h
  · rcases definite_init_sound program h with ⟨certificate, invariant⟩
    refine ⟨certificate, ?_⟩
    intro state reaches instruction fetch running register read
    have current := invariant state reaches
    rcases current with halted | ⟨currentInstruction, currentFetch, currentBoundary, currentFacts⟩
    · cases running.symm.trans halted
    · exact currentFacts register (certificate.readsCovered state.pc instruction fetch register read)


end Bamti.Bytecode
