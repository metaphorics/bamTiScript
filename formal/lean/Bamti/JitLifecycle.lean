import Bamti.NapiLifecycle

namespace Bamti

inductive JitPhase where
  | interpreted
  | queued
  | finalized
  | published
  | retired
  | cancelled
  | failed
  deriving DecidableEq, Repr

inductive WxMemory where
  | unallocated
  | writable
  | executable
  | reclaimed
  deriving DecidableEq, Repr

inductive JitAction where
  | queue (owner : Nat)
  | finalize
  | publish
  | enter
  | osr (ordinal : Nat)
  | resume (ordinal : Nat)
  | beginRead
  | endRead
  | advanceEpoch
  | retire
  | cancel
  | fail
  deriving DecidableEq, Repr

structure Artifact where
  generation : Nat
  phase : JitPhase
  compilerOwner : Option Nat
  arenaSlots : Nat
  readyTokens : Nat
  memory : WxMemory
  generationSnapshot : Option Nat
  fallback : Bool
  readers : Nat
  retireEpoch : Nat
  currentEpoch : Nat
  entryOrdinal : Nat
  entryCount : Nat
  osrOrdinal : Nat
  resumeOrdinal : Nat
  lastTransfer : Option Nat
  deriving DecidableEq, Repr

inductive StepResult where
  | accepted (state : Artifact)
  | waiting (state : Artifact)
  | rejected (state : Artifact)
  deriving DecidableEq, Repr

def reject (s : Artifact) : Artifact :=
  { s with
    phase := .failed
    compilerOwner := none
    arenaSlots := 0
    readyTokens := 0
    memory := .reclaimed
    fallback := true }

def ready (s : Artifact) : Bool :=
  s.phase == .finalized && s.readyTokens == 1

def live (s : Artifact) : Bool :=
  s.phase == .published && s.memory == .executable && !s.fallback

def quiescent (s : Artifact) : Prop :=
  s.readers = 0 ∧ s.retireEpoch < s.currentEpoch

def canQueue (s : Artifact) : Prop :=
  s.phase = .interpreted ∧ s.compilerOwner = none ∧ s.arenaSlots = 0

def canFinalize (s : Artifact) : Prop :=
  s.phase = .queued ∧
    s.compilerOwner ≠ none ∧
    s.arenaSlots = 1 ∧
    s.readyTokens = 0 ∧
    s.memory = .writable

def canPublish (s : Artifact) : Prop :=
  s.phase = .finalized ∧
    s.arenaSlots = 1 ∧
    s.readyTokens = 1 ∧
    s.memory = .executable ∧
    s.generationSnapshot = some s.generation ∧
    s.entryOrdinal = 0 ∧
    s.entryOrdinal < s.entryCount ∧
    s.compilerOwner ≠ none

def canTransfer (s : Artifact) (ordinal : Nat) : Prop :=
  s.phase = .published ∧
    s.memory = .executable ∧
    ordinal < s.entryCount

def canRetire (s : Artifact) : Prop :=
  s.phase = .published ∧ s.memory = .executable ∧ quiescent s

def canWaitForRetirement (s : Artifact) : Prop :=
  s.phase = .published ∧ s.memory = .executable ∧ ¬ quiescent s

def canEndRead (s : Artifact) : Prop :=
  s.phase = .published ∧ 0 < s.readers

inductive JitStep : Artifact → JitAction → StepResult → Prop where
  | queueAccepted (s : Artifact) (owner : Nat) (h : canQueue s) :
      JitStep s (.queue owner)
        (.accepted { s with
          phase := .queued
          compilerOwner := some owner
          arenaSlots := 1
          readyTokens := 0
          memory := .writable
          generationSnapshot := none
          fallback := false })
  | queueRejected (s : Artifact) (owner : Nat) (h : ¬ canQueue s) :
      JitStep s (.queue owner) (.rejected (reject s))
  | finalizeAccepted (s : Artifact) (h : canFinalize s) :
      JitStep s .finalize
        (.accepted { s with
          phase := .finalized
          memory := .executable
          readyTokens := 1
          generationSnapshot := some s.generation })
  | finalizeRejected (s : Artifact) (h : ¬ canFinalize s) :
      JitStep s .finalize (.rejected (reject s))
  | publishAccepted (s : Artifact) (h : canPublish s) :
      JitStep s .publish
        (.accepted { s with
          phase := .published
          compilerOwner := none
          readyTokens := 0
          retireEpoch := s.currentEpoch
          fallback := false })
  | publishRejected (s : Artifact) (h : ¬ canPublish s) :
      JitStep s .publish (.rejected (reject s))
  | enterAccepted (s : Artifact) (h : canTransfer s s.entryOrdinal) :
      JitStep s .enter
        (.accepted { s with
          lastTransfer := some s.entryOrdinal
          fallback := false })
  | enterRejected (s : Artifact) (h : ¬ canTransfer s s.entryOrdinal) :
      JitStep s .enter (.rejected (reject s))
  | osrAccepted (s : Artifact) (ordinal : Nat) (h : canTransfer s ordinal) :
      JitStep s (.osr ordinal)
        (.accepted { s with
          osrOrdinal := ordinal
          lastTransfer := some ordinal
          fallback := false })
  | osrRejected (s : Artifact) (ordinal : Nat) (h : ¬ canTransfer s ordinal) :
      JitStep s (.osr ordinal) (.rejected (reject s))
  | resumeAccepted (s : Artifact) (ordinal : Nat) (h : canTransfer s ordinal) :
      JitStep s (.resume ordinal)
        (.accepted { s with
          resumeOrdinal := ordinal
          lastTransfer := some ordinal
          fallback := false })
  | resumeRejected (s : Artifact) (ordinal : Nat) (h : ¬ canTransfer s ordinal) :
      JitStep s (.resume ordinal) (.rejected (reject s))
  | beginReadAccepted (s : Artifact) (h : s.phase = .published) :
      JitStep s .beginRead (.accepted { s with readers := s.readers + 1 })
  | beginReadRejected (s : Artifact) (h : s.phase ≠ .published) :
      JitStep s .beginRead (.rejected (reject s))
  | endReadAccepted (s : Artifact) (h : canEndRead s) :
      JitStep s .endRead (.accepted { s with readers := s.readers - 1 })
  | endReadRejected (s : Artifact) (h : ¬ canEndRead s) :
      JitStep s .endRead (.rejected (reject s))
  | advanceEpochAccepted (s : Artifact) :
      JitStep s .advanceEpoch (.accepted { s with currentEpoch := s.currentEpoch + 1 })
  | retireAccepted (s : Artifact) (h : canRetire s) :
      JitStep s .retire
        (.accepted { s with
          phase := .retired
          compilerOwner := none
          arenaSlots := 0
          readyTokens := 0
          memory := .reclaimed })
  | retireWaiting (s : Artifact) (h : canWaitForRetirement s) :
      JitStep s .retire (.waiting s)
  | retireRejected (s : Artifact) (h : ¬ (s.phase = .published ∧ s.memory = .executable)) :
      JitStep s .retire (.rejected (reject s))
  | cancelAccepted (s : Artifact) (h : s.phase = .queued ∨ s.phase = .finalized) :
      JitStep s .cancel
        (.accepted { s with
          phase := .cancelled
          compilerOwner := none
          arenaSlots := 0
          readyTokens := 0
          memory := .reclaimed
          fallback := true })
  | cancelRejected (s : Artifact) (h : s.phase ≠ .queued ∧ s.phase ≠ .finalized) :
      JitStep s .cancel (.rejected (reject s))
  | failRejected (s : Artifact) :
      JitStep s .fail (.rejected (reject s))

inductive JitTrace : Artifact → List JitAction → StepResult → Prop where
  | done (s : Artifact) : JitTrace s [] (.accepted s)
  | accepted {s s' : Artifact} {action : JitAction} {actions : List JitAction} {result : StepResult} :
      JitStep s action (.accepted s') →
      JitTrace s' actions result →
      JitTrace s (action :: actions) result
  | waiting {s s' : Artifact} {action : JitAction} {actions : List JitAction} :
      JitStep s action (.waiting s') →
      JitTrace s (action :: actions) (.waiting s')
  | rejected {s s' : Artifact} {action : JitAction} {actions : List JitAction} :
      JitStep s action (.rejected s') →
      JitTrace s (action :: actions) (.rejected s')

theorem queue_bound (s s' : Artifact) (owner : Nat)
    (h : JitStep s (.queue owner) (.accepted s')) :
    s'.phase = .queued ∧ s'.arenaSlots = 1 ∧ s'.readyTokens = 0 ∧ s'.memory = .writable := by
  cases h
  exact ⟨rfl, rfl, rfl, rfl⟩

theorem one_ready_token (s s' : Artifact)
    (h : JitStep s .finalize (.accepted s')) :
    s'.phase = .finalized ∧ ready s' = true ∧ s'.memory = .executable := by
  cases h
  exact ⟨rfl, rfl, rfl⟩

theorem single_compiler_owner (s s' : Artifact) (owner : Nat)
    (h : JitStep s (.queue owner) (.accepted s')) :
    s'.compilerOwner = some owner := by
  cases h
  rfl

theorem slot_conservation (s s' : Artifact)
    (h : JitStep s .publish (.accepted s')) :
    s'.arenaSlots = s.arenaSlots ∧ s'.arenaSlots = 1 := by
  cases h with
  | publishAccepted hp =>
      exact ⟨rfl, hp.2.1⟩

theorem no_publish_before_finalize (s s' : Artifact)
    (h : JitStep s .publish (.accepted s')) :
    s.phase = .finalized := by
  cases h with
  | publishAccepted hp =>
      exact hp.1

theorem wx_before_publish (s s' : Artifact)
    (h : JitStep s .publish (.accepted s')) :
    s.memory = .executable ∧ s.memory ≠ .writable ∧
      s'.memory = .executable ∧ s'.memory ≠ .writable := by
  cases h with
  | publishAccepted hp =>
      simp_all [canPublish]

theorem cancelled_never_live (s s' : Artifact)
    (h : JitStep s .cancel (.accepted s')) :
    s'.phase = .cancelled ∧ live s' = false ∧ s'.fallback = true := by
  cases h
  exact ⟨rfl, by simp [live], rfl⟩

theorem entry_generation_matches_snapshot (s s' : Artifact)
    (h : JitStep s .publish (.accepted s')) :
    s'.generationSnapshot = some s'.generation := by
  cases h with
  | publishAccepted hp =>
      exact hp.2.2.2.2.1

theorem no_reclaim_before_quiescence (s s' : Artifact)
    (h : JitStep s .retire (.waiting s')) :
    s'.phase = .published ∧ s'.memory = .executable ∧ s'.arenaSlots = s.arenaSlots := by
  cases h with
  | retireWaiting hp =>
      exact ⟨hp.1, hp.2.1, rfl⟩

theorem fallback_on_every_reject_or_failure (s s' : Artifact) (action : JitAction)
    (h : JitStep s action (.rejected s')) :
    s'.phase = .failed ∧ s'.fallback = true := by
  cases h <;> simp [reject]

theorem osr_ordinal_in_range (s s' : Artifact) (ordinal : Nat)
    (h : JitStep s (.osr ordinal) (.accepted s')) :
    ordinal < s.entryCount ∧ s'.osrOrdinal = ordinal ∧ s'.lastTransfer = some ordinal := by
  cases h with
  | osrAccepted _ hp =>
      exact ⟨hp.2.2, rfl, rfl⟩

theorem resume_ordinal_stable (s s' : Artifact) (ordinal : Nat)
    (h : JitStep s (.resume ordinal) (.accepted s')) :
    s'.resumeOrdinal = ordinal ∧ s'.entryCount = s.entryCount ∧
      ordinal < s'.entryCount ∧ s'.lastTransfer = some ordinal := by
  cases h with
  | resumeAccepted _ hp =>
      exact ⟨rfl, rfl, hp.2.2, rfl⟩

theorem publication_after_finalization (s s' : Artifact)
    (h : JitStep s .publish (.accepted s')) :
    s'.phase = .published ∧ s'.compilerOwner = none ∧ s'.readyTokens = 0 ∧
      s'.retireEpoch = s.currentEpoch := by
  cases h
  exact ⟨rfl, rfl, rfl, rfl⟩

theorem retirement_waits_for_quiescence (s s' : Artifact)
    (h : JitStep s .retire (.accepted s')) :
    quiescent s := by
  cases h with
  | retireAccepted hp =>
      exact hp.2.2

theorem retirement_after_quiescence (s s' : Artifact)
    (h : JitStep s .retire (.accepted s')) :
    quiescent s ∧ s'.phase = .retired ∧ s'.memory = .reclaimed ∧ s'.arenaSlots = 0 := by
  cases h with
  | retireAccepted hp =>
      exact ⟨hp.2.2, rfl, rfl, rfl⟩

/-- Lifecycle phase shared by all mappings owned by one JIT provider. -/
inductive ProviderPhase where
  | Writable
  | Executable
  | Freed
  deriving DecidableEq, Repr

/-- Operations that can change or inspect the provider phase. -/
inductive ProviderAction where
  | allocate
  | finalize
  | free
  deriving DecidableEq, Repr

/-- Provider operation outcome and its resulting phase. -/
inductive ProviderResult where
  | accepted (next : ProviderPhase)
  | rejected (next : ProviderPhase)
  deriving DecidableEq, Repr

def providerResultState : ProviderResult → ProviderPhase
  | .accepted next => next
  | .rejected next => next

/-- W^X transitions for one provider. Allocation may repeat while writable.
Finalization from the writable phase either publishes executable mappings or
frees every mapping. Finalization from any other phase is refused and leaves
the phase unchanged. Free changes a live phase to freed; later frees are
accepted no-ops. Every action from a phase without an accepted rule is
explicitly rejected, matching the totality pattern of `JitStep` above. -/
inductive ProviderStep : ProviderPhase → ProviderAction → ProviderResult → Prop where
  | allocateAccepted :
      ProviderStep .Writable .allocate (.accepted .Writable)
  | allocateRejected (p : ProviderPhase) (h : p ≠ .Writable) :
      ProviderStep p .allocate (.rejected p)
  | finalizeAccepted :
      ProviderStep .Writable .finalize (.accepted .Executable)
  | finalizeFailed :
      ProviderStep .Writable .finalize (.rejected .Freed)
  | finalizeRejected (p : ProviderPhase) (h : p ≠ .Writable) :
      ProviderStep p .finalize (.rejected p)
  | freeAccepted (p : ProviderPhase) (h : p = .Writable ∨ p = .Executable) :
      ProviderStep p .free (.accepted .Freed)
  | freeIdempotent :
      ProviderStep .Freed .free (.accepted .Freed)

/-- Finite provider traces. -/
inductive ProviderTrace : ProviderPhase → ProviderPhase → Prop where
  | refl (s : ProviderPhase) : ProviderTrace s s
  | step (s u : ProviderPhase) (action : ProviderAction) (result : ProviderResult)
      (hs : ProviderStep s action result)
      (ht : ProviderTrace (providerResultState result) u) :
      ProviderTrace s u

theorem provider_allocation_only_writable (s q : ProviderPhase)
    (h : ProviderStep s .allocate (.accepted q)) :
    s = .Writable ∧ q = .Writable := by
  cases h
  exact ⟨rfl, rfl⟩

theorem provider_finalization_exactly_once (p q : ProviderPhase)
    (h : ProviderStep p .finalize (.accepted q)) :
    p = .Writable ∧ q = .Executable ∧
      ¬ ProviderStep q .finalize (.accepted q) := by
  cases h
  exact ⟨rfl, rfl, fun h2 => by cases h2⟩

/-- A refused finalization never yields a writable phase, and never enables a
later allocation or finalization. It does not imply reclamation; see
`provider_finalization_failure_frees` for the writable-phase failure case,
which is the only refused-finalization branch that reclaims. -/
theorem provider_finalization_failure_never_writable (p q : ProviderPhase)
    (h : ProviderStep p .finalize (.rejected q)) :
    q ≠ .Writable ∧
      (∀ next, ¬ ProviderStep q .allocate (.accepted next)) ∧
      (∀ next, ¬ ProviderStep q .finalize (.accepted next)) := by
  cases h with
  | finalizeFailed =>
      constructor
      · intro hq
        exact ProviderPhase.noConfusion hq
      constructor
      · intro next h2
        cases h2
      · intro next h2
        cases h2
  | finalizeRejected p hp =>
      constructor
      · intro hq
        exact hp hq
      constructor
      · intro next h2
        cases h2
        exact hp rfl
      · intro next h2
        cases h2
        exact hp rfl

/-- Failed finalization of a writable provider frees every mapping: the only
refused-finalization branch that starts from `.Writable` lands in `.Freed`.
This is the reclamation claim; it holds only under the narrower hypothesis
`p = .Writable`, whereas `provider_finalization_failure_never_writable` holds
for any starting phase. -/
theorem provider_finalization_failure_frees (q : ProviderPhase)
    (h : ProviderStep .Writable .finalize (.rejected q)) :
    q = .Freed := by
  cases h with
  | finalizeFailed => rfl
  | finalizeRejected _ hp => exact absurd rfl hp

theorem provider_free_is_idempotent (p q : ProviderPhase)
    (h : ProviderStep p .free (.accepted q)) :
    q = .Freed ∧ ProviderStep q .free (.accepted q) := by
  cases h with
  | freeAccepted _ =>
      exact ⟨rfl, ProviderStep.freeIdempotent⟩
  | freeIdempotent =>
      exact ⟨rfl, ProviderStep.freeIdempotent⟩

/-- An executable provider can only remain executable or become freed along a
reachable trace; once freed, it remains freed.  The second conjunct strengthens
the induction enough to cover a `free` step from the executable phase. -/
theorem provider_trace_exclusivity (s u : ProviderPhase)
    (trace : ProviderTrace s u) :
    (s = .Executable → u = .Executable ∨ u = .Freed) ∧
      (s = .Freed → u = .Freed) := by
  induction trace with
  | refl s =>
      exact ⟨fun h => Or.inl h, fun h => h⟩
  | step s u action result hs ht ih =>
      cases hs with
      | allocateAccepted =>
          constructor <;> intro h <;> cases h
      | finalizeAccepted =>
          constructor <;> intro h <;> cases h
      | finalizeFailed =>
          constructor <;> intro h <;> cases h
      | finalizeRejected p hp =>
          constructor
          · intro h
            exact ih.1 h
          · intro h
            exact ih.2 h
      | allocateRejected p hp =>
          constructor
          · intro h
            exact ih.1 h
          · intro h
            exact ih.2 h
      | freeAccepted p hp =>
          rcases hp with rfl | rfl
          · constructor <;> intro h <;> cases h
          · constructor
            · intro _
              exact Or.inr (ih.2 rfl)
            · intro h
              cases h
      | freeIdempotent =>
          constructor
          · intro h
            cases h
          · intro _
            exact ih.2 rfl

/-- Allocation from an executable provider is rejected, not merely absent.
This is the totality-strengthened W^X guarantee: the model refuses the call
rather than having no rule for it. -/
theorem allocate_from_executable_rejected (p : ProviderPhase)
    (h : p = .Executable) :
    ProviderStep p .allocate (.rejected p) := by
  exact ProviderStep.allocateRejected p (fun hp => ProviderPhase.noConfusion (hp.symm.trans h))

/-- Allocation from a freed provider is rejected. -/
theorem allocate_from_freed_rejected (p : ProviderPhase)
    (h : p = .Freed) :
    ProviderStep p .allocate (.rejected p) := by
  exact ProviderStep.allocateRejected p (fun hp => ProviderPhase.noConfusion (hp.symm.trans h))

/-- Finalization from an executable provider is rejected. -/
theorem finalize_from_executable_rejected (p : ProviderPhase)
    (h : p = .Executable) :
    ProviderStep p .finalize (.rejected p) := by
  exact ProviderStep.finalizeRejected p (fun hp => ProviderPhase.noConfusion (hp.symm.trans h))

/-- Finalization from a freed provider is rejected. -/
theorem finalize_from_freed_rejected (p : ProviderPhase)
    (h : p = .Freed) :
    ProviderStep p .finalize (.rejected p) := by
  exact ProviderStep.finalizeRejected p (fun hp => ProviderPhase.noConfusion (hp.symm.trans h))

/-- No accepted transition is possible from an executable provider except free.
This strengthens `provider_never_writable_executable` from a trace-level
derivation property to a single-step refusal: `allocate` and `finalize`
from `.Executable` are each explicitly rejected by the model. -/
theorem executable_rejects_write_enabling (p : ProviderPhase)
    (h : p = .Executable) :
    (∀ q, ¬ ProviderStep p .allocate (.accepted q)) ∧
      (∀ q, ¬ ProviderStep p .finalize (.accepted q)) := by
  constructor
  · intro q h2
    cases h2
    exact ProviderPhase.noConfusion h
  · intro q h2
    cases h2
    exact ProviderPhase.noConfusion h

/-- No phase reachable from an executable provider is writable. -/
theorem provider_never_writable_executable (s u : ProviderPhase)
    (trace : ProviderTrace s u) :
    s = .Executable → u ≠ .Writable := by
  intro hs hu
  rcases (provider_trace_exclusivity s u trace).1 hs with he | hf
  · rw [hu] at he
    exact ProviderPhase.noConfusion he
  · rw [hu] at hf
    exact ProviderPhase.noConfusion hf

end Bamti
