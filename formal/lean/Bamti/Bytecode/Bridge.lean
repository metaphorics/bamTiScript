import Bamti.Bytecode.Model
import Bamti.Bytecode.Verify
import Bamti.Bytecode.CacheKey

namespace Bamti.Bytecode

/-- Named replay actions are preserved instead of reducing a transition to its ordinal. -/
inductive BridgeAction where
  | interpreterStep
  | verifiedReplay
  deriving DecidableEq, Repr

/-- `formal-trace/v1` carries an action identity and the execution states it relates. -/
structure Trace where
  id : Nat
  formal_model : Nat
  property : Nat
  pre : Program
  post : Program
  action : BridgeAction
  before : MachineState
  after : MachineState
  observable : Nat
  bounds : Nat
  deriving Repr

def preservesBoundaries (trace : Trace) : Prop :=
  ∀ pc, boundary trace.pre pc → boundary trace.post pc

def traceWellFormed (trace : Trace) : Prop :=
  trace.id < trace.bounds ∧
  trace.formal_model < trace.bounds ∧
  trace.property < trace.bounds ∧
  match trace.action with
  | .interpreterStep => trace.before.mode = .running
  | .verifiedReplay => trace.before = trace.after

/-- A replay either records one concrete small step or replays a verified artifact unchanged. -/
def bridgeTransition (trace : Trace) : Prop :=
  match trace.action with
  | .interpreterStep => Executes trace.pre trace.before trace.after ∧ trace.post = trace.pre
  | .verifiedReplay => verifies trace.pre ∧ trace.post = trace.pre ∧ trace.before = trace.after

theorem bridge_trace_preserves_boundaries (trace : Trace) (transition : bridgeTransition trace) :
    preservesBoundaries trace := by
  intro pc preBoundary
  cases action : trace.action with
  | interpreterStep =>
      simp [bridgeTransition, action] at transition
      rcases transition with ⟨_, postEq⟩
      simpa [postEq] using preBoundary
  | verifiedReplay =>
      simp [bridgeTransition, action] at transition
      rcases transition with ⟨_, postEq, _⟩
      simpa [postEq] using preBoundary

end Bamti.Bytecode
