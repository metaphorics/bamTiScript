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
    s.memory = .executable ∧ s'.memory = .executable := by
  cases h with
  | publishAccepted hp =>
      let hMem := hp.2.2.2.1
      exact ⟨hMem, by simpa using hMem⟩

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

end Bamti
