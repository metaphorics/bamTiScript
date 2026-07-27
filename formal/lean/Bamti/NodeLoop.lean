import Bamti.Compiler.Correctness

namespace Bamti

inductive LoopPhase where
  | open
  | closing
  | shut
  deriving DecidableEq, Repr

structure Completion where
  id : Nat
  owner : Nat
  checkpoint : Nat
  deriving DecidableEq, Repr

structure LoopState where
  runtimeOwner : Nat
  callbackOwner : Option Nat
  phase : LoopPhase
  nextId : Nat
  checkpoint : Nat
  microtaskCheckpoint : Nat
  checkpoints : List Nat
  pending : List Completion
  delivered : List Completion
  deriving DecidableEq, Repr

inductive LoopAction where
  | enqueue (callback : Completion)
  | deliver (callbackId : Nat)
  | close
  | shutdown
  deriving DecidableEq, Repr

inductive LoopStepResult where
  | accepted
  | rejected
  deriving DecidableEq, Repr

structure LoopEvent where
  action : LoopAction
  result : LoopStepResult
  deriving DecidableEq, Repr

def callbackIds (callbacks : List Completion) : List Nat :=
  callbacks.map Completion.id

def acceptedDeliveryIds : List LoopEvent → List Nat
  | [] => []
  | event :: rest =>
      match event.result, event.action with
      | .accepted, .deliver callbackId => callbackId :: acceptedDeliveryIds rest
      | _, _ => acceptedDeliveryIds rest

def allBelowBy {α : Type} (key : α → Nat) (bound : Nat) (items : List α) : Prop :=
  ∀ item ∈ items, key item < bound

def strictlyIncreasingBy {α : Type} (key : α → Nat) : List α → Prop
  | [] => True
  | item :: rest =>
      (∀ later ∈ rest, key item < key later) ∧ strictlyIncreasingBy key rest

def callbacksOwnedBy (runtimeOwner : Nat) (callbacks : List Completion) : Prop :=
  ∀ callback ∈ callbacks, callback.owner = runtimeOwner

def callbackCheckpointsBounded (checkpoint : Nat) (callbacks : List Completion) : Prop :=
  ∀ callback ∈ callbacks, callback.checkpoint ≤ checkpoint

def callbackOwnerMatchesRuntime (s : LoopState) : Prop :=
  match s.callbackOwner with
  | none => True
  | some callbackOwner => callbackOwner = s.runtimeOwner

def deliveredExactlyOnce (history : List Completion) : Prop :=
  strictlyIncreasingBy Completion.id history

def loopWellFormed (s : LoopState) : Prop :=
  strictlyIncreasingBy Completion.id (s.delivered ++ s.pending) ∧
    allBelowBy Completion.id s.nextId (s.delivered ++ s.pending) ∧
    callbacksOwnedBy s.runtimeOwner (s.delivered ++ s.pending) ∧
    callbackCheckpointsBounded s.checkpoint (s.delivered ++ s.pending) ∧
    s.microtaskCheckpoint = s.checkpoint ∧
    s.checkpoints = callbackIds s.delivered ∧
    callbackOwnerMatchesRuntime s ∧
    (s.phase = .shut → s.pending = [])

def canEnqueue (s : LoopState) (callback : Completion) : Prop :=
  s.phase = .open ∧
    callback.owner = s.runtimeOwner ∧
    callback.id = s.nextId ∧
    callback.checkpoint = s.checkpoint

def canDeliver (s : LoopState) (callback : Completion) (rest : List Completion) : Prop :=
  s.phase = .open ∧
    s.pending = callback :: rest ∧
    callback.owner = s.runtimeOwner ∧
    callback.checkpoint ≤ s.checkpoint

def enabled (s : LoopState) : LoopAction → Prop
  | .enqueue callback => canEnqueue s callback
  | .deliver callbackId =>
      ∃ callback rest, callback.id = callbackId ∧ canDeliver s callback rest
  | .close => s.phase = .open
  | .shutdown => s.phase = .closing

inductive step : LoopState → LoopEvent → LoopState → Prop where
  | enqueue (s : LoopState) (callback : Completion) (h : canEnqueue s callback) :
      step s { action := .enqueue callback, result := .accepted }
        { s with pending := s.pending ++ [callback], nextId := s.nextId + 1 }
  | deliver (s : LoopState) (callback : Completion) (rest : List Completion)
      (h : canDeliver s callback rest) :
      step s { action := .deliver callback.id, result := .accepted }
        { s with
          pending := rest
          delivered := s.delivered ++ [callback]
          checkpoint := s.checkpoint + 1
          microtaskCheckpoint := s.microtaskCheckpoint + 1
          checkpoints := s.checkpoints ++ [callback.id]
          callbackOwner := some s.runtimeOwner }
  | close (s : LoopState) (h : s.phase = .open) :
      step s { action := .close, result := .accepted }
        { s with phase := .closing, callbackOwner := none }
  | shutdown (s : LoopState) (h : s.phase = .closing) :
      step s { action := .shutdown, result := .accepted }
        { s with phase := .shut, pending := [], callbackOwner := none }
  | reject (s : LoopState) (action : LoopAction) (h : ¬ enabled s action) :
      step s { action := action, result := .rejected } s

inductive Trace : LoopState → List LoopEvent → LoopState → Prop where
  | nil (s : LoopState) : Trace s [] s
  | cons (source middle final : LoopState) (event : LoopEvent) (rest : List LoopEvent)
      (hStep : step source event middle) (hRest : Trace middle rest final) :
      Trace source (event :: rest) final

private theorem allBelowBy_append_single
    {α : Type} {key : α → Nat} {bound : Nat} {items : List α} {item : α}
    (hItems : allBelowBy key bound items) (hItem : key item < bound) :
    allBelowBy key bound (items ++ [item]) := by
  intro candidate hCandidate
  rcases List.mem_append.mp hCandidate with hCandidate | hCandidate
  · exact hItems candidate hCandidate
  · have hCandidateEq : candidate = item := by simpa using hCandidate
    subst candidate
    exact hItem

private theorem allBelowBy_succ
    {α : Type} {key : α → Nat} {bound : Nat} {items : List α}
    (hItems : allBelowBy key bound items) :
    allBelowBy key (bound + 1) items := by
  intro candidate hCandidate
  exact Nat.lt_succ_of_lt (hItems candidate hCandidate)

private theorem allBelowBy_prefix
    {α : Type} {key : α → Nat} {bound : Nat}
    {leading trailing : List α}
    (hItems : allBelowBy key bound (leading ++ trailing)) :
    allBelowBy key bound leading := by
  intro candidate hCandidate
  exact hItems candidate (List.mem_append.mpr (Or.inl hCandidate))

private theorem strictlyIncreasingBy_append_single
    {α : Type} {key : α → Nat} {items : List α} {item : α}
    (hItems : strictlyIncreasingBy key items)
    (hBelow : allBelowBy key (key item) items) :
    strictlyIncreasingBy key (items ++ [item]) := by
  induction items with
  | nil => simp [strictlyIncreasingBy]
  | cons head tail ih =>
      change
        (∀ later ∈ tail, key head < key later) ∧
          strictlyIncreasingBy key tail at hItems
      change
        (∀ later ∈ tail ++ [item], key head < key later) ∧
          strictlyIncreasingBy key (tail ++ [item])
      constructor
      · intro later hLater
        rcases List.mem_append.mp hLater with hLater | hLater
        · exact hItems.1 later hLater
        · have hLaterEq : later = item := by simpa using hLater
          subst later
          exact hBelow head (by simp)
      · have hTail : allBelowBy key (key item) tail := by
          intro later hLater
          exact hBelow later (by simp [hLater])
        exact ih hItems.2 hTail

private theorem strictlyIncreasingBy_prefix
    {α : Type} {key : α → Nat} {leading trailing : List α}
    (hItems : strictlyIncreasingBy key (leading ++ trailing)) :
    strictlyIncreasingBy key leading := by
  induction leading with
  | nil => simp [strictlyIncreasingBy]
  | cons head tail ih =>
      change
        (∀ later ∈ tail ++ trailing, key head < key later) ∧
          strictlyIncreasingBy key (tail ++ trailing) at hItems
      change
        (∀ later ∈ tail, key head < key later) ∧
          strictlyIncreasingBy key tail
      constructor
      · intro later hLater
        exact hItems.1 later (List.mem_append.mpr (Or.inl hLater))
      · exact ih hItems.2

private theorem callbacksOwnedBy_append_single
    {runtimeOwner : Nat} {callbacks : List Completion} {callback : Completion}
    (hCallbacks : callbacksOwnedBy runtimeOwner callbacks)
    (hCallback : callback.owner = runtimeOwner) :
    callbacksOwnedBy runtimeOwner (callbacks ++ [callback]) := by
  intro candidate hCandidate
  rcases List.mem_append.mp hCandidate with hCandidate | hCandidate
  · exact hCallbacks candidate hCandidate
  · have hCandidateEq : candidate = callback := by simpa using hCandidate
    subst candidate
    exact hCallback

private theorem callbacksOwnedBy_prefix
    {runtimeOwner : Nat} {leading trailing : List Completion}
    (hCallbacks : callbacksOwnedBy runtimeOwner (leading ++ trailing)) :
    callbacksOwnedBy runtimeOwner leading := by
  intro candidate hCandidate
  exact hCallbacks candidate (List.mem_append.mpr (Or.inl hCandidate))

private theorem callbackCheckpointsBounded_append_single
    {checkpoint : Nat} {callbacks : List Completion} {callback : Completion}
    (hCallbacks : callbackCheckpointsBounded checkpoint callbacks)
    (hCallback : callback.checkpoint ≤ checkpoint) :
    callbackCheckpointsBounded checkpoint (callbacks ++ [callback]) := by
  intro candidate hCandidate
  rcases List.mem_append.mp hCandidate with hCandidate | hCandidate
  · exact hCallbacks candidate hCandidate
  · have hCandidateEq : candidate = callback := by simpa using hCandidate
    subst candidate
    exact hCallback

private theorem callbackCheckpointsBounded_succ
    {checkpoint : Nat} {callbacks : List Completion}
    (hCallbacks : callbackCheckpointsBounded checkpoint callbacks) :
    callbackCheckpointsBounded (checkpoint + 1) callbacks := by
  intro candidate hCandidate
  exact Nat.le_succ_of_le (hCallbacks candidate hCandidate)

private theorem callbackCheckpointsBounded_prefix
    {checkpoint : Nat} {leading trailing : List Completion}
    (hCallbacks : callbackCheckpointsBounded checkpoint (leading ++ trailing)) :
    callbackCheckpointsBounded checkpoint leading := by
  intro candidate hCandidate
  exact hCallbacks candidate (List.mem_append.mpr (Or.inl hCandidate))

private theorem one_step_preserves_wellFormed
    {source final : LoopState} {event : LoopEvent}
    (hWellFormed : loopWellFormed source) (hStep : step source event final) :
    loopWellFormed final := by
  have hOriginal := hWellFormed
  rcases hWellFormed with
    ⟨hOrdered, hIdsBelow, hOwned, hCheckpointBounded,
      hMicrotask, hCheckpointHistory, hCallbackOwner, hShut⟩
  cases hStep with
  | enqueue callback h =>
      rcases h with ⟨hOpen, hOwner, hId, hCallbackCheckpoint⟩
      have hBelowNewId :
          allBelowBy Completion.id callback.id (source.delivered ++ source.pending) := by
        simpa [hId] using hIdsBelow
      have hNewId : callback.id < source.nextId + 1 := by
        simp [hId]
      have hCallbackBound : callback.checkpoint ≤ source.checkpoint := by
        simp [hCallbackCheckpoint]
      refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
      · simpa [List.append_assoc] using
          strictlyIncreasingBy_append_single hOrdered hBelowNewId
      · simpa [List.append_assoc] using
          allBelowBy_append_single (allBelowBy_succ hIdsBelow) hNewId
      · simpa [List.append_assoc] using
          callbacksOwnedBy_append_single hOwned hOwner
      · simpa [List.append_assoc] using
          callbackCheckpointsBounded_append_single hCheckpointBounded hCallbackBound
      · simpa using hMicrotask
      · simpa using hCheckpointHistory
      · change callbackOwnerMatchesRuntime source
        exact hCallbackOwner
      · intro hShut
        cases hOpen.symm.trans hShut
  | deliver callback rest h =>
      rcases h with ⟨hOpen, hPending, hOwner, hReady⟩
      refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
      · simpa [hPending, List.append_assoc] using hOrdered
      · simpa [hPending, List.append_assoc] using hIdsBelow
      · simpa [hPending, List.append_assoc] using hOwned
      · simpa [hPending, List.append_assoc] using
          callbackCheckpointsBounded_succ hCheckpointBounded
      · exact congrArg (fun checkpoint => checkpoint + 1) hMicrotask
      · simp [callbackIds, hCheckpointHistory]
      · simp [callbackOwnerMatchesRuntime]
      · intro hShut
        cases hOpen.symm.trans hShut
  | close hOpen =>
      refine ⟨hOrdered, hIdsBelow, hOwned, hCheckpointBounded,
        hMicrotask, hCheckpointHistory, ?_, ?_⟩
      · simp [callbackOwnerMatchesRuntime]
      · intro hShut
        cases hShut
  | shutdown hClosing =>
      refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
      · simpa using
          (strictlyIncreasingBy_prefix (leading := source.delivered)
            (trailing := source.pending) hOrdered)
      · simpa using
          (allBelowBy_prefix (leading := source.delivered) (trailing := source.pending)
            hIdsBelow)
      · simpa using
          (callbacksOwnedBy_prefix (leading := source.delivered) (trailing := source.pending)
            hOwned)
      · simpa using
          (callbackCheckpointsBounded_prefix (leading := source.delivered)
            (trailing := source.pending) hCheckpointBounded)
      · simpa using hMicrotask
      · simpa using hCheckpointHistory
      · simp [callbackOwnerMatchesRuntime]
      · intro _
        rfl
  | reject action h =>
      exact hOriginal

theorem step_preserves_wellFormed
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) :
    loopWellFormed source → loopWellFormed final := by
  induction hTrace with
  | nil source =>
      intro hWellFormed
      exact hWellFormed
  | cons source middle final event rest hStep hRest ih =>
      intro hWellFormed
      exact ih (one_step_preserves_wellFormed hWellFormed hStep)

private theorem step_delivered_history
    {source final : LoopState} {event : LoopEvent}
    (hStep : step source event final) :
    callbackIds final.delivered =
      callbackIds source.delivered ++ acceptedDeliveryIds [event] := by
  cases hStep <;> simp [callbackIds, acceptedDeliveryIds]

private theorem acceptedDeliveryIds_cons (event : LoopEvent) (rest : List LoopEvent) :
    acceptedDeliveryIds (event :: rest) =
      acceptedDeliveryIds [event] ++ acceptedDeliveryIds rest := by
  rcases event with ⟨action, result⟩
  cases action <;> cases result <;> rfl

private theorem trace_delivery_history
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) :
    callbackIds final.delivered =
      callbackIds source.delivered ++ acceptedDeliveryIds events := by
  induction hTrace with
  | nil source =>
      simp [acceptedDeliveryIds]
  | cons source middle final event rest hStep hRest ih =>
      calc
        callbackIds final.delivered =
            callbackIds middle.delivered ++ acceptedDeliveryIds rest := ih
        _ = (callbackIds source.delivered ++ acceptedDeliveryIds [event]) ++
            acceptedDeliveryIds rest := by
              rw [step_delivered_history hStep]
        _ = callbackIds source.delivered ++ acceptedDeliveryIds (event :: rest) := by
              rw [acceptedDeliveryIds_cons event rest]
              simp only [List.append_assoc]

theorem completion_fifo
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) (hWellFormed : loopWellFormed source) :
    strictlyIncreasingBy Completion.id (final.delivered ++ final.pending) ∧
      callbackIds final.delivered =
        callbackIds source.delivered ++ acceptedDeliveryIds events := by
  exact ⟨(step_preserves_wellFormed hTrace hWellFormed).1, trace_delivery_history hTrace⟩

theorem completion_exactly_once
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) (hWellFormed : loopWellFormed source) :
    deliveredExactlyOnce final.delivered ∧
      final.checkpoints = callbackIds final.delivered ∧
      callbackIds final.delivered =
        callbackIds source.delivered ++ acceptedDeliveryIds events := by
  rcases step_preserves_wellFormed hTrace hWellFormed with
    ⟨hOrdered, hIdsBelow, hOwned, hCheckpointBounded,
      hMicrotask, hCheckpointHistory, hCallbackOwner, hShut⟩
  refine ⟨?_, hCheckpointHistory, (completion_fifo hTrace hWellFormed).2⟩
  simpa [deliveredExactlyOnce] using
    (strictlyIncreasingBy_prefix (leading := final.delivered) (trailing := final.pending)
      hOrdered)

private theorem after_close_not_open
    {s : LoopState}
    (hAfterClose : s.phase = .closing ∨ s.phase = .shut)
    (hOpen : s.phase = .open) : False := by
  rcases hAfterClose with hClosing | hShut
  · cases hClosing.symm.trans hOpen
  · cases hShut.symm.trans hOpen

private theorem shut_not_open
    {s : LoopState} (hShut : s.phase = .shut) (hOpen : s.phase = .open) : False := by
  cases hShut.symm.trans hOpen

private theorem shut_not_closing
    {s : LoopState} (hShut : s.phase = .shut) (hClosing : s.phase = .closing) : False := by
  cases hShut.symm.trans hClosing

private theorem step_after_close
    {source final : LoopState} {event : LoopEvent}
    (hAfterClose : source.phase = .closing ∨ source.phase = .shut)
    (hNoActiveCallback : source.callbackOwner = none)
    (hStep : step source event final) :
    final.delivered = source.delivered ∧
      final.checkpoints = source.checkpoints ∧
      final.checkpoint = source.checkpoint ∧
      final.microtaskCheckpoint = source.microtaskCheckpoint ∧
      final.callbackOwner = none ∧
      (final.phase = .closing ∨ final.phase = .shut) := by
  cases hStep with
  | enqueue callback h =>
      exact False.elim (after_close_not_open hAfterClose h.1)
  | deliver callback rest h =>
      exact False.elim (after_close_not_open hAfterClose h.1)
  | close hOpen =>
      exact False.elim (after_close_not_open hAfterClose hOpen)
  | shutdown hClosing =>
      rcases hAfterClose with hClosingPhase | hShut
      · exact ⟨rfl, rfl, rfl, rfl, rfl, Or.inr rfl⟩
      · exact False.elim (shut_not_closing hShut hClosing)
  | reject action h =>
      exact ⟨rfl, rfl, rfl, rfl, hNoActiveCallback, hAfterClose⟩

private theorem close_trace_preserves
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) :
    (source.phase = .closing ∨ source.phase = .shut) →
      source.callbackOwner = none →
      final.delivered = source.delivered ∧
        final.checkpoints = source.checkpoints ∧
        final.checkpoint = source.checkpoint ∧
        final.microtaskCheckpoint = source.microtaskCheckpoint ∧
        final.callbackOwner = none ∧
        (final.phase = .closing ∨ final.phase = .shut) := by
  induction hTrace with
  | nil source =>
      intro hAfterClose hNoActiveCallback
      exact ⟨rfl, rfl, rfl, rfl, hNoActiveCallback, hAfterClose⟩
  | cons source middle final event rest hStep hRest ih =>
      intro hAfterClose hNoActiveCallback
      rcases step_after_close hAfterClose hNoActiveCallback hStep with
        ⟨hDelivered, hCheckpoints, hCheckpoint, hMicrotask,
          hCallbackOwner, hMiddlePhase⟩
      rcases ih hMiddlePhase hCallbackOwner with
        ⟨hFinalDelivered, hFinalCheckpoints, hFinalCheckpoint, hFinalMicrotask,
          hFinalCallbackOwner, hFinalPhase⟩
      exact ⟨hFinalDelivered.trans hDelivered, hFinalCheckpoints.trans hCheckpoints,
        hFinalCheckpoint.trans hCheckpoint, hFinalMicrotask.trans hMicrotask,
        hFinalCallbackOwner, hFinalPhase⟩

theorem no_callback_after_close
    {beforeClose afterClose final : LoopState} {future : List LoopEvent}
    (hClose : step beforeClose { action := .close, result := .accepted } afterClose)
    (hFuture : Trace afterClose future final) :
    final.delivered = afterClose.delivered ∧
      final.checkpoints = afterClose.checkpoints ∧
      final.checkpoint = afterClose.checkpoint ∧
      final.microtaskCheckpoint = afterClose.microtaskCheckpoint ∧
      final.callbackOwner = none ∧
      (final.phase = .closing ∨ final.phase = .shut) := by
  cases hClose with
  | close hOpen =>
      exact close_trace_preserves hFuture (Or.inl rfl) rfl

private theorem step_after_shutdown
    {source final : LoopState} {event : LoopEvent}
    (hShut : source.phase = .shut)
    (hNoPending : source.pending = [])
    (hNoActiveCallback : source.callbackOwner = none)
    (hStep : step source event final) :
    final.phase = .shut ∧
      final.pending = [] ∧
      final.delivered = source.delivered ∧
      final.checkpoints = source.checkpoints ∧
      final.checkpoint = source.checkpoint ∧
      final.microtaskCheckpoint = source.microtaskCheckpoint ∧
      final.callbackOwner = none := by
  cases hStep with
  | enqueue callback h =>
      exact False.elim (shut_not_open hShut h.1)
  | deliver callback rest h =>
      exact False.elim (shut_not_open hShut h.1)
  | close hOpen =>
      exact False.elim (shut_not_open hShut hOpen)
  | shutdown hClosing =>
      exact False.elim (shut_not_closing hShut hClosing)
  | reject action h =>
      exact ⟨hShut, hNoPending, rfl, rfl, rfl, rfl, hNoActiveCallback⟩

private theorem shutdown_trace_stable
    {source final : LoopState} {events : List LoopEvent}
    (hTrace : Trace source events final) :
    source.phase = .shut →
      source.pending = [] →
      source.callbackOwner = none →
      final.phase = .shut ∧
        final.pending = [] ∧
        final.delivered = source.delivered ∧
        final.checkpoints = source.checkpoints ∧
        final.checkpoint = source.checkpoint ∧
        final.microtaskCheckpoint = source.microtaskCheckpoint ∧
        final.callbackOwner = none := by
  induction hTrace with
  | nil source =>
      intro hShut hNoPending hNoActiveCallback
      exact ⟨hShut, hNoPending, rfl, rfl, rfl, rfl, hNoActiveCallback⟩
  | cons source middle final event rest hStep hRest ih =>
      intro hShut hNoPending hNoActiveCallback
      rcases step_after_shutdown hShut hNoPending hNoActiveCallback hStep with
        ⟨hMiddlePhase, hMiddlePending, hDelivered, hCheckpoints, hCheckpoint,
          hMicrotask, hCallbackOwner⟩
      rcases ih hMiddlePhase hMiddlePending hCallbackOwner with
        ⟨hFinalPhase, hFinalPending, hFinalDelivered, hFinalCheckpoints,
          hFinalCheckpoint, hFinalMicrotask, hFinalCallbackOwner⟩
      exact ⟨hFinalPhase, hFinalPending, hFinalDelivered.trans hDelivered,
        hFinalCheckpoints.trans hCheckpoints, hFinalCheckpoint.trans hCheckpoint,
        hFinalMicrotask.trans hMicrotask, hFinalCallbackOwner⟩

theorem shutdown_no_future_user_callback
    {beforeShutdown afterShutdown final : LoopState} {future : List LoopEvent}
    (hShutdown :
      step beforeShutdown { action := .shutdown, result := .accepted } afterShutdown)
    (hFuture : Trace afterShutdown future final) :
    final.phase = .shut ∧
      final.pending = [] ∧
      final.delivered = afterShutdown.delivered ∧
      final.checkpoints = afterShutdown.checkpoints ∧
      final.checkpoint = afterShutdown.checkpoint ∧
      final.microtaskCheckpoint = afterShutdown.microtaskCheckpoint ∧
      final.callbackOwner = none := by
  cases hShutdown with
  | shutdown hClosing =>
      exact shutdown_trace_stable hFuture rfl rfl rfl

def initialState (runtimeOwner : Nat) : LoopState :=
  { runtimeOwner := runtimeOwner
    callbackOwner := none
    phase := .open
    nextId := 0
    checkpoint := 0
    microtaskCheckpoint := 0
    checkpoints := []
    pending := []
    delivered := [] }

def initialCallback (runtimeOwner : Nat) : Completion :=
  { id := 0, owner := runtimeOwner, checkpoint := 0 }

def normalEvents (runtimeOwner : Nat) : List LoopEvent :=
  [{ action := .enqueue (initialCallback runtimeOwner), result := .accepted },
    { action := .deliver 0, result := .accepted }]

def afterEnqueue (runtimeOwner : Nat) : LoopState :=
  { runtimeOwner := runtimeOwner
    callbackOwner := none
    phase := .open
    nextId := 1
    checkpoint := 0
    microtaskCheckpoint := 0
    checkpoints := []
    pending := [initialCallback runtimeOwner]
    delivered := [] }

def afterNormal (runtimeOwner : Nat) : LoopState :=
  { runtimeOwner := runtimeOwner
    callbackOwner := some runtimeOwner
    phase := .open
    nextId := 1
    checkpoint := 1
    microtaskCheckpoint := 1
    checkpoints := [0]
    pending := []
    delivered := [initialCallback runtimeOwner] }

theorem initial_wellFormed (runtimeOwner : Nat) :
    loopWellFormed (initialState runtimeOwner) := by
  simp [initialState, loopWellFormed, strictlyIncreasingBy, allBelowBy,
    callbacksOwnedBy, callbackCheckpointsBounded, callbackOwnerMatchesRuntime,
    callbackIds]

theorem normal_delivery_trace (runtimeOwner : Nat) :
    Trace (initialState runtimeOwner) (normalEvents runtimeOwner) (afterNormal runtimeOwner) := by
  change Trace (initialState runtimeOwner)
    [{ action := .enqueue (initialCallback runtimeOwner), result := .accepted },
      { action := .deliver 0, result := .accepted }]
    (afterNormal runtimeOwner)
  refine Trace.cons (initialState runtimeOwner) (afterEnqueue runtimeOwner)
    (afterNormal runtimeOwner)
    { action := .enqueue (initialCallback runtimeOwner), result := .accepted }
    [{ action := .deliver 0, result := .accepted }] ?_ ?_
  · apply step.enqueue
    exact ⟨rfl, rfl, rfl, rfl⟩
  · refine Trace.cons (afterEnqueue runtimeOwner) (afterNormal runtimeOwner)
      (afterNormal runtimeOwner)
      { action := .deliver 0, result := .accepted } [] ?_ ?_
    · have hCan : canDeliver (afterEnqueue runtimeOwner) (initialCallback runtimeOwner) [] := by
        simp [canDeliver, afterEnqueue, initialCallback]
      simpa [afterEnqueue, afterNormal, initialCallback] using
        (step.deliver (afterEnqueue runtimeOwner) (initialCallback runtimeOwner) [] hCan)
    · exact Trace.nil (afterNormal runtimeOwner)

end Bamti
