import Bamti.Compiler.Relation

namespace Bamti.Compiler

/-- Verification accepts only complete opcode/report pairs; malformed streams are excluded. -/
def verified (machine : Machine) : Prop :=
  VerifiedCode machine.code

theorem compile_verified (source : Source) : verified (compile source) := by
  exact lowerProgram_verified source.forms

/-- The empty program is a genuine terminal simulation, including its visible history. -/
theorem simulation_stop (history : List Observable) :
    sourceOutcome (sourceStep (emptySourceState history)) =
      observation (bytecodeRun 2 (compileState (emptySourceState history))) := by
  simpa [observation] using
    (related_outcomes _ _ (compile_state_step_commutes (emptySourceState history)))

/-- The legacy tick catalog row now witnesses the first real source form translation. -/
theorem simulation_tick (history : List Observable) :
    related (sourceStep (singleFormState .literalExpr history))
      (bytecodeRun 2 (compileState (singleFormState .literalExpr history))) :=
  compile_state_step_commutes (singleFormState .literalExpr history)

/-- Lift the concrete two-instruction simulation to arbitrarily many source transitions. -/
theorem weak_bisimulation_runs (steps : Nat) (sourceState : SourceState)
    (bytecodeState : BytecodeState) (h : related sourceState bytecodeState) :
    related (sourceRun steps sourceState) (bytecodeWeakRun steps bytecodeState) := by
  induction steps generalizing sourceState bytecodeState with
  | zero =>
      simpa [sourceRun, bytecodeWeakRun] using h
  | succ steps ih =>
      simpa [sourceRun, bytecodeWeakRun] using
        ih (sourceStep sourceState) (bytecodeRun 2 bytecodeState)
          (weak_step_simulation sourceState bytecodeState h)

/-- Compilation relates every finite source run to its real, two-step-per-form interpreter run. -/
theorem compile_core_weak_bisim (steps : Nat) (state : SourceState) :
    related (sourceRun steps state) (bytecodeWeakRun steps (compileState state)) :=
  weak_bisimulation_runs steps state (compileState state) (compileState_related state)

/-- The translation preserves the full effect trace and terminal observable for every finite run. -/
theorem compile_core_observational_equivalence (steps : Nat) (state : SourceState) :
    sourceObservations steps state = bytecodeWeakObservations steps (compileState state) ∧
      sourceOutcome (sourceRun steps state) =
        observation (bytecodeWeakRun steps (compileState state)) := by
  have h := compile_core_weak_bisim steps state
  constructor
  · exact h.2.symm
  · exact related_outcomes _ _ h

/-- One-form simulations state the target's concrete visible report, not only a relation label. -/
theorem simulate_form (form : SourceForm) (history : List Observable) :
    related (sourceStep (singleFormState form history))
      (bytecodeRun 2 (compileState (singleFormState form history))) ∧
      (bytecodeRun 2 (compileState (singleFormState form history))).trace =
        history ++ [ruleObservable (sourceRule form)] := by
  have h := compile_state_step_commutes (singleFormState form history)
  constructor
  · exact h
  · simpa [singleFormState, sourceStep, srcStep] using h.2

theorem simulate_literal (history : List Observable) :
    related (sourceStep (singleFormState .literalExpr history))
      (bytecodeRun 2 (compileState (singleFormState .literalExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .literalExpr history))).trace =
        history ++ [.normal .literal] := by
  simpa [sourceRule, ruleObservable] using simulate_form .literalExpr history

theorem simulate_variable (history : List Observable) :
    related (sourceStep (singleFormState .variableExpr history))
      (bytecodeRun 2 (compileState (singleFormState .variableExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .variableExpr history))).trace =
        history ++ [.normal .«variable»] := by
  simpa [sourceRule, ruleObservable] using simulate_form .variableExpr history

theorem simulate_property_get (history : List Observable) :
    related (sourceStep (singleFormState .propertyGetExpr history))
      (bytecodeRun 2 (compileState (singleFormState .propertyGetExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .propertyGetExpr history))).trace =
        history ++ [.normal .property_get] := by
  simpa [sourceRule, ruleObservable] using simulate_form .propertyGetExpr history

theorem simulate_property_set (history : List Observable) :
    related (sourceStep (singleFormState .propertySetExpr history))
      (bytecodeRun 2 (compileState (singleFormState .propertySetExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .propertySetExpr history))).trace =
        history ++ [.normal .property_set] := by
  simpa [sourceRule, ruleObservable] using simulate_form .propertySetExpr history

theorem simulate_call (history : List Observable) :
    related (sourceStep (singleFormState .callExpr history))
      (bytecodeRun 2 (compileState (singleFormState .callExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .callExpr history))).trace =
        history ++ [.normal .call] := by
  simpa [sourceRule, ruleObservable] using simulate_form .callExpr history

theorem simulate_construct (history : List Observable) :
    related (sourceStep (singleFormState .constructExpr history))
      (bytecodeRun 2 (compileState (singleFormState .constructExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .constructExpr history))).trace =
        history ++ [.normal .construct] := by
  simpa [sourceRule, ruleObservable] using simulate_form .constructExpr history

theorem simulate_conditional (history : List Observable) :
    related (sourceStep (singleFormState .conditionalExpr history))
      (bytecodeRun 2 (compileState (singleFormState .conditionalExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .conditionalExpr history))).trace =
        history ++ [.normal .conditional] := by
  simpa [sourceRule, ruleObservable] using simulate_form .conditionalExpr history

theorem simulate_sequence (history : List Observable) :
    related (sourceStep (singleFormState .sequenceExpr history))
      (bytecodeRun 2 (compileState (singleFormState .sequenceExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .sequenceExpr history))).trace =
        history ++ [.normal .sequence] := by
  simpa [sourceRule, ruleObservable] using simulate_form .sequenceExpr history

theorem simulate_loop (history : List Observable) :
    related (sourceStep (singleFormState .loopExpr history))
      (bytecodeRun 2 (compileState (singleFormState .loopExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .loopExpr history))).trace =
        history ++ [.normal .«loop»] := by
  simpa [sourceRule, ruleObservable] using simulate_form .loopExpr history

theorem simulate_throw (history : List Observable) :
    related (sourceStep (singleFormState .throwExpr history))
      (bytecodeRun 2 (compileState (singleFormState .throwExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .throwExpr history))).trace =
        history ++ [.thrown .«throw»] := by
  simpa [sourceRule, ruleObservable] using simulate_form .throwExpr history

theorem simulate_try_catch_finally (history : List Observable) :
    related (sourceStep (singleFormState .tryCatchFinallyExpr history))
      (bytecodeRun 2 (compileState (singleFormState .tryCatchFinallyExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .tryCatchFinallyExpr history))).trace =
        history ++ [.normal .try_catch_finally] := by
  simpa [sourceRule, ruleObservable] using simulate_form .tryCatchFinallyExpr history

theorem simulate_iterator (history : List Observable) :
    related (sourceStep (singleFormState .iteratorExpr history))
      (bytecodeRun 2 (compileState (singleFormState .iteratorExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .iteratorExpr history))).trace =
        history ++ [.suspended .iterator] := by
  simpa [sourceRule, ruleObservable] using simulate_form .iteratorExpr history

theorem simulate_promise (history : List Observable) :
    related (sourceStep (singleFormState .promiseExpr history))
      (bytecodeRun 2 (compileState (singleFormState .promiseExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .promiseExpr history))).trace =
        history ++ [.suspended .promise] := by
  simpa [sourceRule, ruleObservable] using simulate_form .promiseExpr history

theorem simulate_async (history : List Observable) :
    related (sourceStep (singleFormState .asyncExpr history))
      (bytecodeRun 2 (compileState (singleFormState .asyncExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .asyncExpr history))).trace =
        history ++ [.suspended .«async»] := by
  simpa [sourceRule, ruleObservable] using simulate_form .asyncExpr history

theorem simulate_binding (history : List Observable) :
    related (sourceStep (singleFormState .bindingExpr history))
      (bytecodeRun 2 (compileState (singleFormState .bindingExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .bindingExpr history))).trace =
        history ++ [.normal .binding] := by
  simpa [sourceRule, ruleObservable] using simulate_form .bindingExpr history

theorem simulate_module_link (history : List Observable) :
    related (sourceStep (singleFormState .moduleLinkExpr history))
      (bytecodeRun 2 (compileState (singleFormState .moduleLinkExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .moduleLinkExpr history))).trace =
        history ++ [.moduleEffect .module_link] := by
  simpa [sourceRule, ruleObservable] using simulate_form .moduleLinkExpr history

theorem simulate_module_evaluate (history : List Observable) :
    related (sourceStep (singleFormState .moduleEvaluateExpr history))
      (bytecodeRun 2 (compileState (singleFormState .moduleEvaluateExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .moduleEvaluateExpr history))).trace =
        history ++ [.moduleEffect .module_evaluate] := by
  simpa [sourceRule, ruleObservable] using simulate_form .moduleEvaluateExpr history

theorem simulate_dynamic_import (history : List Observable) :
    related (sourceStep (singleFormState .dynamicImportExpr history))
      (bytecodeRun 2 (compileState (singleFormState .dynamicImportExpr history))) ∧
      (bytecodeRun 2 (compileState (singleFormState .dynamicImportExpr history))).trace =
        history ++ [.moduleEffect .dynamic_import] := by
  simpa [sourceRule, ruleObservable] using simulate_form .dynamicImportExpr history

end Bamti.Compiler
