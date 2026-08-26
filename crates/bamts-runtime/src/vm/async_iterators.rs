//! Pure transition policies for async generators and async-from-sync iterators.
//!
//! Runtime queues, activations, promises, and iterator objects remain owned by
//! the VM. This module only selects the next operation for those existing
//! carriers, so every protocol decision can be tested without another state
//! machine or backing store.

use bamts_native::Value;

use crate::{AsyncGeneratorState, ThrowOrigin};

use super::generator_async::{GeneratorCompletion, GeneratorMethod};

/// Non-owning view of the runtime's async-generator lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AsyncGeneratorPhase {
    SuspendedStart,
    SuspendedYield,
    Executing,
    AwaitingOperand,
    AwaitingYield,
    AwaitingResumption,
    AwaitingReturn,
    Completed,
}

impl From<&AsyncGeneratorState> for AsyncGeneratorPhase {
    fn from(state: &AsyncGeneratorState) -> Self {
        match state {
            AsyncGeneratorState::SuspendedStart(_) => Self::SuspendedStart,
            AsyncGeneratorState::SuspendedYield(_) => Self::SuspendedYield,
            AsyncGeneratorState::Executing => Self::Executing,
            AsyncGeneratorState::AwaitingOperand(_) => Self::AwaitingOperand,
            AsyncGeneratorState::AwaitingYield(_) => Self::AwaitingYield,
            AsyncGeneratorState::AwaitingResumption(_) => Self::AwaitingResumption,
            AsyncGeneratorState::AwaitingReturn => Self::AwaitingReturn,
            AsyncGeneratorState::Completed => Self::Completed,
        }
    }
}

/// Work selected for the request at the front of an async-generator queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AsyncGeneratorPlan {
    /// The body or an await reaction owns the generator. Keep FIFO order and do
    /// not reject synchronously as a synchronous generator would on reentry.
    EnqueueOnly,
    /// Resume the suspended activation with the request's typed completion.
    Drive(GeneratorCompletion),
    /// Await a return operand before resuming the suspended yield activation.
    AwaitResumption(Value),
    /// Start a never-entered body. The first sent value is ignored.
    StartDrive,
    /// Await a return value without ever entering a never-started body.
    AwaitReturn(Value),
    /// Complete a never-started generator and reject the front request.
    SettleThrow { value: Value, origin: ThrowOrigin },
    /// Resolve a completed generator's next request with undefined and done.
    DrainNext,
    /// Reject a completed generator's throw request.
    DrainThrow { value: Value, origin: ThrowOrigin },
}

/// Total async-generator queue transition table.
///
/// `GeneratorCompletion` is also the request kind: `Normal` is `.next`, while
/// `Return` and `Throw` retain their abrupt-completion semantics.
pub(crate) const fn async_generator_plan(
    phase: AsyncGeneratorPhase,
    completion: GeneratorCompletion,
) -> AsyncGeneratorPlan {
    use AsyncGeneratorPhase::{
        AwaitingOperand, AwaitingResumption, AwaitingReturn, AwaitingYield, Completed, Executing,
        SuspendedStart, SuspendedYield,
    };

    match (phase, completion) {
        (Executing | AwaitingOperand | AwaitingYield | AwaitingResumption | AwaitingReturn, _) => {
            AsyncGeneratorPlan::EnqueueOnly
        }

        (SuspendedStart, GeneratorCompletion::Normal(_)) => AsyncGeneratorPlan::StartDrive,
        (SuspendedStart, GeneratorCompletion::Return(value)) => {
            AsyncGeneratorPlan::AwaitReturn(value)
        }
        (SuspendedStart, GeneratorCompletion::Throw { value, origin }) => {
            AsyncGeneratorPlan::SettleThrow { value, origin }
        }

        (SuspendedYield, GeneratorCompletion::Return(value)) => {
            AsyncGeneratorPlan::AwaitResumption(value)
        }
        (SuspendedYield, completion) => AsyncGeneratorPlan::Drive(completion),

        (Completed, GeneratorCompletion::Normal(_)) => AsyncGeneratorPlan::DrainNext,
        (Completed, GeneratorCompletion::Return(value)) => AsyncGeneratorPlan::AwaitReturn(value),
        (Completed, GeneratorCompletion::Throw { value, origin }) => {
            AsyncGeneratorPlan::DrainThrow { value, origin }
        }
    }
}

/// `%AsyncGeneratorPrototype%` method descriptors, in installation order.
pub(crate) const ASYNC_GENERATOR_METHODS: [GeneratorMethod; 3] = [
    GeneratorMethod {
        name: "next",
        length: 1,
    },
    GeneratorMethod {
        name: "return",
        length: 1,
    },
    GeneratorMethod {
        name: "throw",
        length: 1,
    },
];

/// `%AsyncFromSyncIteratorPrototype%` method descriptors, in installation order.
pub(crate) const ASYNC_FROM_SYNC_METHODS: [GeneratorMethod; 3] = ASYNC_GENERATOR_METHODS;

/// Maps a prototype handler to the typed completion consumed by the existing
/// async-generator request queue.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeFunction, SuspendedActivation};
    use bamts_bytecode::{FunctionId, ModuleId};

    fn number(value: u32) -> Value {
        Value::int32(value)
    }

    fn completions(value: Value, origin: ThrowOrigin) -> [GeneratorCompletion; 3] {
        [
            GeneratorCompletion::Normal(value),
            GeneratorCompletion::Return(value),
            GeneratorCompletion::Throw { value, origin },
        ]
    }

    fn dummy_activation() -> SuspendedActivation {
        SuspendedActivation {
            target: RuntimeFunction {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
            },
            registers: Vec::new(),
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            resume_token: 1,
            context: None,
        }
    }

    #[test]
    fn transition_table_covers_every_phase_and_request_kind() {
        let sent = number(17);
        let origin = ThrowOrigin::Bytecode;
        let phases = [
            AsyncGeneratorPhase::SuspendedStart,
            AsyncGeneratorPhase::SuspendedYield,
            AsyncGeneratorPhase::Executing,
            AsyncGeneratorPhase::AwaitingOperand,
            AsyncGeneratorPhase::AwaitingYield,
            AsyncGeneratorPhase::AwaitingResumption,
            AsyncGeneratorPhase::AwaitingReturn,
            AsyncGeneratorPhase::Completed,
        ];
        let expected = [
            [
                AsyncGeneratorPlan::StartDrive,
                AsyncGeneratorPlan::AwaitReturn(sent),
                AsyncGeneratorPlan::SettleThrow {
                    value: sent,
                    origin,
                },
            ],
            [
                AsyncGeneratorPlan::Drive(GeneratorCompletion::Normal(sent)),
                AsyncGeneratorPlan::AwaitResumption(sent),
                AsyncGeneratorPlan::Drive(GeneratorCompletion::Throw {
                    value: sent,
                    origin,
                }),
            ],
            [AsyncGeneratorPlan::EnqueueOnly; 3],
            [AsyncGeneratorPlan::EnqueueOnly; 3],
            [AsyncGeneratorPlan::EnqueueOnly; 3],
            [AsyncGeneratorPlan::EnqueueOnly; 3],
            [AsyncGeneratorPlan::EnqueueOnly; 3],
            [
                AsyncGeneratorPlan::DrainNext,
                AsyncGeneratorPlan::AwaitReturn(sent),
                AsyncGeneratorPlan::DrainThrow {
                    value: sent,
                    origin,
                },
            ],
        ];

        let mut cells = 0;
        for (phase, expected_row) in phases.into_iter().zip(expected) {
            for (completion, expected_plan) in
                completions(sent, origin).into_iter().zip(expected_row)
            {
                assert_eq!(async_generator_plan(phase, completion), expected_plan);
                cells += 1;
            }
        }
        assert_eq!(cells, 8 * 3);
    }

    #[test]
    fn busy_phases_enqueue_every_request_without_reentrancy_failure() {
        let value = number(1);
        for phase in [
            AsyncGeneratorPhase::Executing,
            AsyncGeneratorPhase::AwaitingOperand,
            AsyncGeneratorPhase::AwaitingYield,
            AsyncGeneratorPhase::AwaitingResumption,
            AsyncGeneratorPhase::AwaitingReturn,
        ] {
            for completion in completions(value, ThrowOrigin::Bytecode) {
                assert_eq!(
                    async_generator_plan(phase, completion),
                    AsyncGeneratorPlan::EnqueueOnly
                );
            }
        }
    }

    #[test]
    fn awaiting_resumption_projects_to_distinct_phase_and_enqueues() {
        let value = number(4);
        let origin = ThrowOrigin::Bytecode;
        let state = AsyncGeneratorState::AwaitingResumption(dummy_activation());
        assert_eq!(
            AsyncGeneratorPhase::from(&state),
            AsyncGeneratorPhase::AwaitingResumption
        );
        for completion in completions(value, origin) {
            assert_eq!(
                async_generator_plan(AsyncGeneratorPhase::AwaitingResumption, completion),
                AsyncGeneratorPlan::EnqueueOnly
            );
        }
    }

    #[test]
    fn start_return_awaits_and_start_throw_preserves_origin() {
        let value = number(2);
        let origin = ThrowOrigin::TypeError {
            operation: "throw-origin",
        };
        assert_eq!(
            async_generator_plan(
                AsyncGeneratorPhase::SuspendedStart,
                GeneratorCompletion::Return(value)
            ),
            AsyncGeneratorPlan::AwaitReturn(value)
        );
        assert_eq!(
            async_generator_plan(
                AsyncGeneratorPhase::SuspendedStart,
                GeneratorCompletion::Throw { value, origin }
            ),
            AsyncGeneratorPlan::SettleThrow { value, origin }
        );
    }

    #[test]
    fn completed_requests_drain_by_operation() {
        let value = number(3);
        let origin = ThrowOrigin::ReferenceError {
            operation: "completed-throw",
        };
        assert_eq!(
            async_generator_plan(
                AsyncGeneratorPhase::Completed,
                GeneratorCompletion::Normal(value)
            ),
            AsyncGeneratorPlan::DrainNext
        );
        assert_eq!(
            async_generator_plan(
                AsyncGeneratorPhase::Completed,
                GeneratorCompletion::Return(value)
            ),
            AsyncGeneratorPlan::AwaitReturn(value)
        );
        assert_eq!(
            async_generator_plan(
                AsyncGeneratorPhase::Completed,
                GeneratorCompletion::Throw { value, origin }
            ),
            AsyncGeneratorPlan::DrainThrow { value, origin }
        );
    }

    #[test]
    fn method_tables_cover_next_return_throw() {
        let methods = [
            GeneratorMethod {
                name: "next",
                length: 1,
            },
            GeneratorMethod {
                name: "return",
                length: 1,
            },
            GeneratorMethod {
                name: "throw",
                length: 1,
            },
        ];
        assert_eq!(ASYNC_GENERATOR_METHODS, methods);
        assert_eq!(ASYNC_FROM_SYNC_METHODS, methods);
    }
}
