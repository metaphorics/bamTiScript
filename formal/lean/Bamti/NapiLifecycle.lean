import Bamti.NodeLoop

namespace Bamti

/-- Node-API teardown moves through these phases in declaration order. -/
inductive TeardownPhase where
  | active
  | closing
  | closed
  deriving DecidableEq, Repr

def phaseRank : TeardownPhase → Nat
  | .active => 0
  | .closing => 1
  | .closed => 2

def phaseLe (before after : TeardownPhase) : Prop :=
  phaseRank before ≤ phaseRank after

structure NapiRoot where
  owner : Nat
  value : Nat
  deriving DecidableEq, Repr

structure NapiState where
  env : Nat
  owner : Nat
  roots : List NapiRoot
  completed : Bool
  finalized : Bool
  phase : TeardownPhase
  scopeDepth : Nat
  refCount : Nat
  asyncPending : Nat
  tsfnPending : Nat
  finalizerQueued : Bool
  generation : Nat
  handleGeneration : Nat
  handleLive : Bool
  deriving Repr

def rootsOwnedBy (s : NapiState) : Prop :=
  ∀ root ∈ s.roots, root.owner = s.env

def handleResolves (s : NapiState) : Prop :=
  s.handleLive = true ∧ s.handleGeneration = s.generation

def teardownDrained (s : NapiState) : Prop :=
  s.scopeDepth = 0 ∧
    s.refCount = 0 ∧
      s.asyncPending = 0 ∧
        s.tsfnPending = 0 ∧
          s.finalizerQueued = false

def napiWellFormed (s : NapiState) : Prop :=
  s.owner = s.env ∧
    rootsOwnedBy s ∧
      (s.handleLive = true → s.handleGeneration = s.generation) ∧
        (s.finalized = true → s.finalizerQueued = false) ∧
          (s.phase = .closed →
            s.roots = [] ∧
              s.scopeDepth = 0 ∧
                s.refCount = 0 ∧
                  s.asyncPending = 0 ∧
                    s.tsfnPending = 0 ∧
                      s.finalizerQueued = false ∧
                        s.handleLive = false)

def initialNapi (env : Nat) : NapiState :=
  { env := env
    owner := env
    roots := []
    completed := false
    finalized := false
    phase := .active
    scopeDepth := 0
    refCount := 0
    asyncPending := 0
    tsfnPending := 0
    finalizerQueued := false
    generation := 0
    handleGeneration := 0
    handleLive := true }

def finish (s : NapiState) : NapiState :=
  { s with completed := true }

def finalize (s : NapiState) : NapiState :=
  { s with finalizerQueued := false, finalized := true }

def retiredGeneration (s : NapiState) : Nat :=
  if s.handleLive = true then s.generation + 1 else s.generation

def closeNapi (s : NapiState) : NapiState :=
  { s with
    phase := .closed
    roots := []
    scopeDepth := 0
    refCount := 0
    asyncPending := 0
    tsfnPending := 0
    finalizerQueued := false
    generation := retiredGeneration s
    handleLive := false }

inductive NapiAction where
  | openScope
  | closeScope
  | createRoot (value : Nat)
  | dropRoot
  | addRef
  | dropRef
  | queueAsync
  | completeAsync
  | acquireTsfn
  | releaseTsfn
  | queueFinalizer
  | runFinalizer
  | complete
  | retireHandle
  | beginTeardown
  | finishTeardown
  deriving DecidableEq, Repr

/-- A transition is present only when the API operation changes lifecycle state. -/
inductive NapiTransition : NapiState → NapiAction → NapiState → Prop where
  | openScope {s : NapiState} (active : s.phase = .active) :
      NapiTransition s .openScope { s with scopeDepth := s.scopeDepth + 1 }
  | closeScope {s : NapiState} (notClosed : s.phase ≠ .closed) (hasScope : 0 < s.scopeDepth) :
      NapiTransition s .closeScope { s with scopeDepth := s.scopeDepth - 1 }
  | createRoot {s : NapiState} (value : Nat) (active : s.phase = .active) :
      NapiTransition s (.createRoot value)
        { s with roots := { owner := s.env, value := value } :: s.roots }
  | dropRoot {s : NapiState} {root : NapiRoot} {rest : List NapiRoot}
      (notClosed : s.phase ≠ .closed) (hasRoot : s.roots = root :: rest) :
      NapiTransition s .dropRoot { s with roots := rest }
  | addRef {s : NapiState} (active : s.phase = .active) :
      NapiTransition s .addRef { s with refCount := s.refCount + 1 }
  | dropRef {s : NapiState} (notClosed : s.phase ≠ .closed) (hasRef : 0 < s.refCount) :
      NapiTransition s .dropRef { s with refCount := s.refCount - 1 }
  | queueAsync {s : NapiState} (active : s.phase = .active) :
      NapiTransition s .queueAsync { s with asyncPending := s.asyncPending + 1 }
  | completeAsync {s : NapiState} (notClosed : s.phase ≠ .closed) (pending : 0 < s.asyncPending) :
      NapiTransition s .completeAsync { s with asyncPending := s.asyncPending - 1 }
  | acquireTsfn {s : NapiState} (active : s.phase = .active) :
      NapiTransition s .acquireTsfn { s with tsfnPending := s.tsfnPending + 1 }
  | releaseTsfn {s : NapiState} (notClosed : s.phase ≠ .closed) (pending : 0 < s.tsfnPending) :
      NapiTransition s .releaseTsfn { s with tsfnPending := s.tsfnPending - 1 }
  | queueFinalizer {s : NapiState} (active : s.phase = .active)
      (notQueued : s.finalizerQueued = false) (notFinalized : s.finalized = false) :
      NapiTransition s .queueFinalizer { s with finalizerQueued := true }
  | runFinalizer {s : NapiState} (notClosed : s.phase ≠ .closed)
      (queued : s.finalizerQueued = true) (notFinalized : s.finalized = false) :
      NapiTransition s .runFinalizer (finalize s)
  | complete {s : NapiState} (notClosed : s.phase ≠ .closed) (notCompleted : s.completed = false) :
      NapiTransition s .complete (finish s)
  | retireHandle {s : NapiState} (notClosed : s.phase ≠ .closed) (live : s.handleLive = true) :
      NapiTransition s .retireHandle
        { s with generation := s.generation + 1, handleLive := false }
  | beginTeardown {s : NapiState} (active : s.phase = .active) :
      NapiTransition s .beginTeardown { s with phase := .closing }
  | finishTeardown {s : NapiState} (closing : s.phase = .closing) (drained : teardownDrained s) :
      NapiTransition s .finishTeardown (closeNapi s)

inductive NapiTrace : NapiState → List NapiAction → NapiState → Prop where
  | nil {s : NapiState} : NapiTrace s [] s
  | cons {s t u : NapiState} {history : List NapiAction} (action : NapiAction) :
      NapiTransition s action t →
      NapiTrace t history u →
      NapiTrace s (action :: history) u

theorem initial_napi_wellFormed (env : Nat) : napiWellFormed (initialNapi env) := by
  simp [initialNapi, napiWellFormed, rootsOwnedBy]

theorem phaseLe_refl (phase : TeardownPhase) : phaseLe phase phase :=
  Nat.le_refl _

theorem phaseLe_trans {a b c : TeardownPhase} :
    phaseLe a b → phaseLe b c → phaseLe a c := by
  intro hab hbc
  exact Nat.le_trans hab hbc

theorem transition_phase_monotone {s t : NapiState} {action : NapiAction}
    (transition : NapiTransition s action t) : phaseLe s.phase t.phase := by
  cases transition <;> simp_all [phaseLe, phaseRank, finish, finalize, closeNapi]

theorem transition_preserves_owner {s t : NapiState} {action : NapiAction}
    (transition : NapiTransition s action t) (owned : s.owner = s.env) :
    t.owner = t.env := by
  cases transition <;> simpa [finish, finalize, closeNapi] using owned

theorem transition_preserves_root_ownership {s t : NapiState} {action : NapiAction}
    (transition : NapiTransition s action t) (owned : rootsOwnedBy s) :
    rootsOwnedBy t := by
  cases transition <;> simp_all [rootsOwnedBy, finish, finalize, closeNapi]

theorem trace_preserves_owner {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) (owned : s.owner = s.env) : t.owner = t.env := by
  induction trace with
  | nil => exact owned
  | cons _ transition _ ih =>
      exact ih (transition_preserves_owner transition owned)

theorem trace_preserves_root_ownership {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) (owned : rootsOwnedBy s) : rootsOwnedBy t := by
  induction trace with
  | nil => exact owned
  | cons _ transition _ ih =>
      exact ih (transition_preserves_root_ownership transition owned)

theorem transition_preserves_completed {s t : NapiState} {action : NapiAction}
    (transition : NapiTransition s action t) (completed : s.completed = true) :
    t.completed = true := by
  cases transition <;> simp_all [finalize, closeNapi]

theorem transition_complete_is_fresh {s t : NapiState}
    (transition : NapiTransition s .complete t) :
    s.completed = false ∧ t.completed = true := by
  cases transition <;> simp_all [finish]

theorem trace_has_no_complete_after_completion {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) (completed : s.completed = true) :
    NapiAction.complete ∉ history := by
  induction trace with
  | nil => simp
  | cons action transition tail ih =>
      intro occurs
      rcases List.mem_cons.mp occurs with head | later
      · subst action
        have fresh := transition_complete_is_fresh transition
        have impossible : False := by
          simpa [completed] using fresh.1
        exact impossible.elim
      · exact ih (transition_preserves_completed transition completed) later

theorem transition_preserves_finalized {s t : NapiState} {action : NapiAction}
    (transition : NapiTransition s action t) (finalized : s.finalized = true) :
    t.finalized = true := by
  cases transition <;> simp_all [finish, closeNapi]

theorem transition_finalizer_is_fresh {s t : NapiState}
    (transition : NapiTransition s .runFinalizer t) :
    s.finalized = false ∧ t.finalized = true := by
  cases transition <;> simp_all [finalize]

theorem trace_has_no_finalizer_after_finalization {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) (finalized : s.finalized = true) :
    NapiAction.runFinalizer ∉ history := by
  induction trace with
  | nil => simp
  | cons action transition tail ih =>
      intro occurs
      rcases List.mem_cons.mp occurs with head | later
      · subst action
        have fresh := transition_finalizer_is_fresh transition
        have impossible : False := by
          simpa [finalized] using fresh.1
        exact impossible.elim
      · exact ih (transition_preserves_finalized transition finalized) later

theorem no_cross_env_root {s t : NapiState} {history : List NapiAction}
    (initial : napiWellFormed s) (trace : NapiTrace s history t) :
    t.owner = t.env ∧ ∀ root ∈ t.roots, root.owner = t.env :=
  ⟨trace_preserves_owner trace initial.1,
    trace_preserves_root_ownership trace initial.2.1⟩

theorem at_most_once_complete {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) : history.count NapiAction.complete ≤ 1 := by
  induction trace with
  | nil => simp
  | cons action transition tail ih =>
      cases action with
      | complete =>
          have fresh := transition_complete_is_fresh transition
          have noLater := trace_has_no_complete_after_completion tail fresh.2
          have tailCount := List.count_eq_zero.mpr noLater
          simp [tailCount]
      | openScope => simpa using ih
      | closeScope => simpa using ih
      | createRoot _ => simpa using ih
      | dropRoot => simpa using ih
      | addRef => simpa using ih
      | dropRef => simpa using ih
      | queueAsync => simpa using ih
      | completeAsync => simpa using ih
      | acquireTsfn => simpa using ih
      | releaseTsfn => simpa using ih
      | queueFinalizer => simpa using ih
      | runFinalizer => simpa using ih
      | retireHandle => simpa using ih
      | beginTeardown => simpa using ih
      | finishTeardown => simpa using ih

theorem at_most_once_finalizer {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) : history.count NapiAction.runFinalizer ≤ 1 := by
  induction trace with
  | nil => simp
  | cons action transition tail ih =>
      cases action with
      | runFinalizer =>
          have fresh := transition_finalizer_is_fresh transition
          have noLater := trace_has_no_finalizer_after_finalization tail fresh.2
          have tailCount := List.count_eq_zero.mpr noLater
          simp [tailCount]
      | openScope => simpa using ih
      | closeScope => simpa using ih
      | createRoot _ => simpa using ih
      | dropRoot => simpa using ih
      | addRef => simpa using ih
      | dropRef => simpa using ih
      | queueAsync => simpa using ih
      | completeAsync => simpa using ih
      | acquireTsfn => simpa using ih
      | releaseTsfn => simpa using ih
      | queueFinalizer => simpa using ih
      | complete => simpa using ih
      | retireHandle => simpa using ih
      | beginTeardown => simpa using ih
      | finishTeardown => simpa using ih

theorem retire_handle_invalidates_snapshot {s t : NapiState}
    (transition : NapiTransition s .retireHandle t) : ¬ handleResolves t := by
  cases transition <;> simp [handleResolves]

theorem trace_phase_monotone {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) : phaseLe s.phase t.phase := by
  induction trace with
  | nil => exact phaseLe_refl _
  | cons _ transition _ ih =>
      exact phaseLe_trans (transition_phase_monotone transition) ih

theorem teardown_phase_monotone {s t : NapiState} {history : List NapiAction}
    (trace : NapiTrace s history t) : phaseLe s.phase t.phase :=
  trace_phase_monotone trace

end Bamti
