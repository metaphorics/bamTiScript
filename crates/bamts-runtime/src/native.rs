//! The native semantic engine.
//!
//! [`NativeEngine`] is the runtime side of the native-execution ABI. It
//! implements [`bamts_native::NativeOps`] — the panic- and nesting-safe seam the
//! generated `bamts_*` helpers dispatch into — by reusing the *exact* heap,
//! global, host, and value-semantic methods of the interpreter [`Machine`]. It
//! adds no second copy of any JavaScript semantic: `dispatch` funnels every one
//! of the 30 helpers (which cover all 36 opcodes) into the shared [`Machine`]
//! methods, so the interpreter and the native engine agree by construction.
//!
//! What the engine owns beyond the shared semantics is exactly the layer the
//! interpreter's dispatch loop cannot be reused for: its own **activation
//! records** (`this`/`arguments`/`new.target`, resume state), **register
//! lifetime** (each frame's register file is a driver-stack local, never
//! engine state, so a validated [`NativeFrame`] view borrows it disjointly),
//! **completions/handlers**, **suspend/resume**, and **stdout/exit**.
//!
//! # `&self` dispatch and interior mutability
//!
//! [`NativeOps::dispatch`] takes `&self`: a re-entrant helper (a nested `Call`
//! or accessor invocation) dispatches back into the *same* engine on the same
//! thread while an outer `dispatch` is still on the stack. A `&mut self`
//! receiver would make those nested reborrows aliasing UB, so all mutable
//! engine state lives behind [`RefCell`]/[`Cell`]. The invariant that keeps this
//! sound and panic-free: **no borrow guard is ever held across a nested call**
//! ([`NativeEntryTable::invoke`], [`NativeEngine::execute`], or
//! [`NativeEngine::invoke_callee`]). Every `machine`/`activations` borrow is
//! taken, used, and dropped before the nested call begins.
//!
//! # Two backends, one semantic core
//!
//! * **Reference** ([`NativeEngine::run`]) — the compiler-free driver. It walks
//!   verified bytecode, and for every value/heap/host operation constructs the
//!   [`HelperCall`] the code generator would emit and routes it through
//!   [`NativeOps::dispatch`]. Control flow (branches, calls, handler search,
//!   suspend/resume) is the driver's, standing in for the CLIF the AOT/JIT
//!   backend generates. It never invokes [`Machine::run`] or its loop.
//! * **Linked** ([`run_linked_program`]) — the real AOT/JIT path. It installs
//!   the engine as the thread's [`NativeOps`] with
//!   [`bamts_native::with_native_ops`] and invokes compiled entries through a
//!   borrowed [`bamts_native::NativeEntryTable`]; the compiled code calls the
//!   `bamts_*` helpers, which dispatch back into the same engine.
//!
//! Both backends share the identical `dispatch`/`truthy` implementation.
//!
//! No `unsafe` lives here: [`bamts_native::ShadowFrame::new`] and
//! [`bamts_native::NativeFrame::new`] are safe constructors, and all raw-pointer
//! dereferencing stays inside `bamts-native`.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;

use bamts_bytecode::{ConstantId, FunctionId, Instruction, Module, ModuleId, Program, Verified};
pub use bamts_native::AbiError;
use bamts_native::{
    Completion, CompletionTag, HelperCall, HelperResult, NativeEntryTable, NativeFrame, NativeOps,
    ShadowFrame, Value, with_native_ops,
};

use crate::intrinsics::BuiltinOutcome;
use crate::{
    CalleeKind, EvalFailure, Execution, ExecutionOutcome, GetOutcome, HeapEntry, Host, Limits,
    Machine, PropertyMap, RuntimeError, RuntimeErrorKind, SetOutcome, ThrowOrigin,
    accessor_from_selector, binary_from_selector, iterator_kind_from_selector, unary_from_selector,
};

// -- ABI selector encoders (inverse of the shared `*_from_selector` decoders) --
//
// The reference driver holds a `bamts_bytecode` operator enum and must produce
// the raw `u32` selector the code generator would pass, so the value round-trips
// through the same `HelperCall`/`dispatch` seam generated code uses. The mapping
// is byte-identical to `bamts_codegen`'s `*_op_selector` functions.

fn unary_to_selector(op: bamts_bytecode::UnaryOp) -> u32 {
    use bamts_bytecode::UnaryOp;
    match op {
        UnaryOp::Void => 0,
        UnaryOp::TypeOf => 1,
        UnaryOp::Plus => 2,
        UnaryOp::Negate => 3,
        UnaryOp::BitwiseNot => 4,
        UnaryOp::LogicalNot => 5,
    }
}

fn binary_to_selector(op: bamts_bytecode::BinaryOp) -> u32 {
    use bamts_bytecode::BinaryOp;
    match op {
        BinaryOp::Add => 0,
        BinaryOp::Subtract => 1,
        BinaryOp::Multiply => 2,
        BinaryOp::Divide => 3,
        BinaryOp::Remainder => 4,
        BinaryOp::Exponent => 5,
        BinaryOp::BitAnd => 6,
        BinaryOp::BitOr => 7,
        BinaryOp::BitXor => 8,
        BinaryOp::ShiftLeft => 9,
        BinaryOp::ShiftRight => 10,
        BinaryOp::UnsignedShiftRight => 11,
        BinaryOp::Equal => 12,
        BinaryOp::NotEqual => 13,
        BinaryOp::StrictEqual => 14,
        BinaryOp::StrictNotEqual => 15,
        BinaryOp::LessThan => 16,
        BinaryOp::LessThanOrEqual => 17,
        BinaryOp::GreaterThan => 18,
        BinaryOp::GreaterThanOrEqual => 19,
        BinaryOp::InstanceOf => 20,
        BinaryOp::In => 21,
    }
}

fn iterator_kind_to_selector(kind: bamts_bytecode::IteratorKind) -> u32 {
    use bamts_bytecode::IteratorKind;
    match kind {
        IteratorKind::Sync => 0,
        IteratorKind::Async => 1,
        IteratorKind::Keys => 2,
    }
}

fn accessor_to_selector(kind: bamts_bytecode::AccessorKind) -> u32 {
    use bamts_bytecode::AccessorKind;
    match kind {
        AccessorKind::Getter => 0,
        AccessorKind::Setter => 1,
    }
}

// -- Errors ------------------------------------------------------------------

/// A native-execution failure: a runtime error, an AOT/entry linkage error, or
/// an unrecoverable native trap whose completion value carries a trap id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeError {
    /// A deterministic runtime error (throw, limit, or malformed value).
    Runtime(RuntimeError),
    /// An AOT image or entry-table linkage failure.
    Abi(AbiError),
    /// The entry table was not compiled from the supplied program's canonical bytes.
    ProgramMismatch,
    /// A native entry returned `FatalTrap`; `value` is the raw trap id word.
    FatalTrap { value: Value },
}

impl fmt::Display for NativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeError::Runtime(error) => write!(formatter, "{error}"),
            NativeError::Abi(error) => write!(formatter, "{error}"),
            NativeError::ProgramMismatch => {
                write!(
                    formatter,
                    "native entries were compiled from different program bytes"
                )
            }
            NativeError::FatalTrap { value } => {
                write!(
                    formatter,
                    "native fatal trap (value {:#018x})",
                    value.to_bits()
                )
            }
        }
    }
}

impl Error for NativeError {}

impl From<RuntimeError> for NativeError {
    fn from(error: RuntimeError) -> Self {
        NativeError::Runtime(error)
    }
}

impl From<AbiError> for NativeError {
    fn from(error: AbiError) -> Self {
        NativeError::Abi(error)
    }
}

// -- Engine state ------------------------------------------------------------

/// Which entry-invocation seam a nested runtime call re-enters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    /// The compiler-free driver: nested calls recurse into [`NativeEngine::execute`].
    Reference,
    /// Compiled entries: nested calls go through the [`NativeEntryTable`].
    Linked,
}

/// A native activation record. It holds only per-call *metadata*; the register
/// file lives as a driver-stack-local `Vec<Value>` so a [`NativeFrame`] can
/// borrow it disjointly from the engine.
struct Activation {
    this_value: Value,
    new_target: Value,
    args: Vec<Value>,
    arguments_object: Option<Value>,
    /// The resumed value delivered to a pending `ResumeValue` (linked backend).
    pending_resume: Option<Value>,
}

/// A thrown value together with its origin, threaded out of `dispatch` through
/// the engine (the native completion ABI carries only a value).
#[derive(Clone, Copy)]
struct PendingThrow {
    value: Value,
    origin: ThrowOrigin,
}

/// How a single driven instruction resolves.
enum Flow {
    /// Advance to the next instruction.
    Next,
    /// Jump to a program counter.
    Goto(usize),
    /// No covering handler; unwind this frame with the thrown value.
    Unwind(Value, ThrowOrigin),
}

/// The observable result of driving one activation to termination.
enum FrameCompletion {
    Normal(Value),
    Unwind(Value, ThrowOrigin, usize),
}

/// The result of invoking a callee (runtime function, host entity, or foreign
/// value).
enum InvokeOutcome {
    Value(Value),
    Threw(Value, ThrowOrigin),
    /// An unrecoverable error; the engine's pending trap state is set.
    Fatal,
}

/// The native semantic engine over a verified program.
///
/// `'m` bounds the module and entry table; `'h` bounds the host borrow. The
/// embedded [`Machine`] owns the shared heap/globals/host/limits and every
/// value-semantic method; the engine never calls [`Machine::run`]. All mutable
/// state is behind [`RefCell`]/[`Cell`] because [`NativeOps`] dispatches on
/// `&self` (see the module docs on re-entrancy).
pub struct NativeEngine<'m, 'h, H: Host> {
    /// The shared semantic core: heap, globals, host, limits, and the one live
    /// module registry used by interpreter and native execution alike.
    machine: RefCell<Machine<'h, H>>,
    /// The verified program whose module-local bytecode drives reference calls.
    program: &'m Program<Verified>,
    /// The compiled entry table, used by the linked backend and nested calls.
    entries: &'m dyn NativeEntryTable,
    backend: Backend,
    /// Metadata for each live activation; register files are driver-stack locals.
    activations: RefCell<Vec<Activation>>,
    /// Live register accounting mirroring the interpreter's ceiling.
    live_registers: Cell<usize>,
    /// Remaining instruction budget for the reference driver.
    fuel: Cell<u64>,
    /// Buffered process stdout.
    stdout: RefCell<Vec<u8>>,
    /// The process exit code (`0` for normal termination).
    exit_code: Cell<i32>,
    /// A throw's value+origin, set by `dispatch`, consumed by the driver.
    pending_throw: Cell<Option<PendingThrow>>,
    /// A shallow fatal kind, set by `dispatch`; the driver attaches source.
    pending_fatal_kind: Cell<Option<RuntimeErrorKind>>,
    /// A fully-sourced error from a nested activation, propagated verbatim.
    pending_error: Cell<Option<RuntimeError>>,
    /// A nested entry-table linkage failure, propagated through `FatalTrap`.
    pending_abi_error: Cell<Option<AbiError>>,
}

impl<'m, 'h, H: Host> NativeEngine<'m, 'h, H> {
    fn build(
        program: &'m Program<Verified>,
        entries: &'m dyn NativeEntryTable,
        host: &'h mut H,
        limits: Limits,
        backend: Backend,
    ) -> Self
    where
        'm: 'h,
    {
        let fuel = limits.fuel;
        NativeEngine {
            machine: RefCell::new(Machine::new(program, host, limits)),
            program,
            entries,
            backend,
            activations: RefCell::new(Vec::new()),
            live_registers: Cell::new(0),
            fuel: Cell::new(fuel),
            stdout: RefCell::new(Vec::new()),
            exit_code: Cell::new(0),
            pending_throw: Cell::new(None),
            pending_fatal_kind: Cell::new(None),
            pending_error: Cell::new(None),
            pending_abi_error: Cell::new(None),
        }
    }

    /// A native engine that drives control flow itself (the compiler-free
    /// reference backend). `entries` is used only for nested calls under the
    /// linked backend; the reference backend recurses internally.
    #[must_use]
    pub fn new(
        program: &'m Program<Verified>,
        entries: &'m dyn NativeEntryTable,
        host: &'h mut H,
        limits: Limits,
    ) -> Self
    where
        'm: 'h,
    {
        Self::build(program, entries, host, limits, Backend::Reference)
    }

    /// Buffered process stdout produced during execution.
    #[must_use]
    pub fn stdout(&self) -> Vec<u8> {
        self.stdout.borrow().clone()
    }

    /// The process exit code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_code.get()
    }

    fn max_call_depth(&self) -> usize {
        self.machine.borrow().limits.max_call_depth
    }

    fn max_total_registers(&self) -> usize {
        self.machine.borrow().limits.max_total_registers
    }

    fn fuel_limit(&self) -> u64 {
        self.machine.borrow().limits.fuel
    }

    fn error_at(
        &self,
        module: ModuleId,
        kind: RuntimeErrorKind,
        function: usize,
        pc: usize,
    ) -> RuntimeError {
        self.machine
            .borrow()
            .error_at_in_module(kind, module, function, pc)
    }

    fn module(&self, module: ModuleId) -> &Module<Verified> {
        &self
            .program
            .module(module)
            .expect("verified native module id remains in bounds")
            .code
    }

    // -- Reference backend: control-flow driver ------------------------------

    /// Executes the program entry with the reference driver. Never invokes
    /// [`Machine::run`].
    pub fn run(self) -> Result<Execution, RuntimeError> {
        self.machine.borrow_mut().instantiate_modules()?;
        self.evaluate_reference_module(self.program.entry())?
            .ok_or_else(|| {
                let module = self.program.entry();
                let function = self.module(module).entry().get() as usize;
                self.error_at(
                    module,
                    RuntimeErrorKind::InvalidVerifiedProgram {
                        module,
                        instruction: Instruction::Halt,
                    },
                    function,
                    0,
                )
            })
    }

    fn evaluate_reference_module(
        &self,
        module: ModuleId,
    ) -> Result<Option<Execution>, RuntimeError> {
        let dependencies = match self.machine.borrow_mut().begin_module_evaluation(module)? {
            crate::ModuleEvaluation::Cycle => return Ok(None),
            crate::ModuleEvaluation::Evaluated(result) => return result.map(Some),
            crate::ModuleEvaluation::Ready(dependencies) => dependencies,
        };
        for dependency in dependencies {
            if let Err(error) = self.evaluate_reference_module(dependency) {
                return self
                    .machine
                    .borrow_mut()
                    .finish_module_evaluation(module, Err(error))
                    .map(Some);
            }
        }

        let code = self.module(module);
        let function = code.entry().get() as usize;
        let register_count = code.functions()[function].register_count() as usize;
        let result = if self.max_call_depth() < 1 {
            Err(self.error_at(
                module,
                RuntimeErrorKind::CallDepthExceeded {
                    limit: self.max_call_depth(),
                },
                function,
                0,
            ))
        } else if register_count > self.max_total_registers() {
            Err(self.error_at(
                module,
                RuntimeErrorKind::RegisterLimitExceeded {
                    limit: self.max_total_registers(),
                },
                function,
                0,
            ))
        } else {
            self.execute(
                module,
                function,
                Value::UNDEFINED,
                Value::UNDEFINED,
                Vec::new(),
                &[],
            )
            .and_then(|(completion, registers)| match completion {
                FrameCompletion::Normal(value) => Ok(Execution {
                    outcome: ExecutionOutcome {
                        stdout: self.stdout.borrow().clone(),
                        exit_code: self.exit_code.get(),
                    },
                    value,
                    link: value,
                    entry_registers: registers,
                }),
                FrameCompletion::Unwind(value, origin, pc) => Err(self.error_at(
                    module,
                    RuntimeErrorKind::UncaughtThrow { value, origin },
                    function,
                    pc,
                )),
            })
        };
        self.machine
            .borrow_mut()
            .finish_module_evaluation(module, result)
            .map(Some)
    }

    /// Seeds a fresh register file: leading captures, then parameters, the rest
    /// uninitialized — identical to the interpreter's frame prologue.
    fn seed_registers(
        &self,
        module: ModuleId,
        function: usize,
        captures: &[Value],
        args: &[Value],
    ) -> Vec<Value> {
        let metadata = &self.module(module).functions()[function];
        let register_count = metadata.register_count() as usize;
        let capture_count = metadata.capture_count() as usize;
        let parameter_count = metadata.parameter_count() as usize;
        let mut registers = vec![Value::UNINITIALIZED; register_count];
        for index in 0..capture_count {
            if let Some(slot) = registers.get_mut(index) {
                *slot = captures.get(index).copied().unwrap_or(Value::UNDEFINED);
            }
        }
        for index in 0..parameter_count {
            if let Some(slot) = registers.get_mut(capture_count + index) {
                *slot = args.get(index).copied().unwrap_or(Value::UNDEFINED);
            }
        }
        registers
    }

    /// Drives one activation to termination. Pushes the activation metadata,
    /// runs the instruction loop over a driver-local register file, and returns
    /// both the completion and the final register file (the caller ignores the
    /// registers except for the entry frame).
    fn execute(
        &self,
        module: ModuleId,
        function: usize,
        this_value: Value,
        new_target: Value,
        args: Vec<Value>,
        captures: &[Value],
    ) -> Result<(FrameCompletion, Vec<Value>), RuntimeError> {
        let register_count = self.module(module).functions()[function].register_count() as usize;
        let mut registers = self.seed_registers(module, function, captures, &args);
        self.live_registers
            .set(self.live_registers.get() + register_count);
        self.activations.borrow_mut().push(Activation {
            this_value,
            new_target,
            args,
            arguments_object: None,
            pending_resume: None,
        });
        let completion = self.run_frame(module, function, &mut registers);
        self.activations.borrow_mut().pop();
        self.live_registers
            .set(self.live_registers.get() - register_count);
        completion.map(|completion| (completion, registers))
    }

    /// The instruction loop for one activation. `registers` is the caller-owned
    /// register file; a [`NativeFrame`] borrows it disjointly from `self`.
    fn run_frame(
        &self,
        module: ModuleId,
        function: usize,
        registers: &mut Vec<Value>,
    ) -> Result<FrameCompletion, RuntimeError> {
        let length = u16::try_from(registers.len()).map_err(|_| {
            let limit = self.max_total_registers();
            self.error_at(
                module,
                RuntimeErrorKind::RegisterLimitExceeded { limit },
                function,
                0,
            )
        })?;
        let handles = registers.as_mut_ptr();
        let mut shadow = ShadowFrame::new(std::ptr::null_mut(), 0, module.get(), handles, length);
        let mut frame =
            NativeFrame::new(&mut shadow, registers.as_mut_slice()).ok_or_else(|| {
                self.error_at(
                    module,
                    RuntimeErrorKind::InvalidValue {
                        value: Value::UNDEFINED,
                    },
                    function,
                    0,
                )
            })?;

        let mut pc = 0usize;
        loop {
            if self.fuel.get() == 0 {
                let limit = self.fuel_limit();
                return Err(self.error_at(
                    module,
                    RuntimeErrorKind::FuelExhausted { limit },
                    function,
                    pc,
                ));
            }
            self.fuel.set(self.fuel.get() - 1);
            let instruction = self.module(module).functions()[function].code()[pc];

            // Control-flow opcodes are the driver's own responsibility (the CLIF
            // the AOT/JIT backend generates); every value/heap/host opcode is
            // routed through `dispatch` into the shared semantics.
            match instruction {
                Instruction::Move { dst, src } => {
                    let value = frame.register(src.get());
                    frame.set_register(dst.get(), value);
                    pc += 1;
                }
                Instruction::Jump { target } => pc = target.get() as usize,
                Instruction::JumpIfTrue { condition, target } => {
                    let value = frame.register(condition.get());
                    pc = if self.truthy(&mut frame, value) {
                        target.get() as usize
                    } else {
                        pc + 1
                    };
                }
                Instruction::JumpIfFalse { condition, target } => {
                    let value = frame.register(condition.get());
                    pc = if self.truthy(&mut frame, value) {
                        pc + 1
                    } else {
                        target.get() as usize
                    };
                }
                Instruction::Return { value } => {
                    return Ok(FrameCompletion::Normal(frame.register(value.get())));
                }
                Instruction::Halt => return Ok(FrameCompletion::Normal(Value::UNDEFINED)),
                Instruction::Throw { value } => {
                    let thrown = frame.register(value.get());
                    match self.raise(
                        &mut frame,
                        module,
                        function,
                        pc,
                        thrown,
                        ThrowOrigin::Bytecode,
                    ) {
                        Flow::Next => pc += 1,
                        Flow::Goto(target) => pc = target,
                        Flow::Unwind(value, origin) => {
                            return Ok(FrameCompletion::Unwind(value, origin, pc));
                        }
                    }
                }
                Instruction::Suspend { .. } => {
                    match self.raise(
                        &mut frame,
                        module,
                        function,
                        pc,
                        Value::UNDEFINED,
                        ThrowOrigin::TypeError {
                            operation: "suspend outside an engine-owned event loop",
                        },
                    ) {
                        Flow::Next => pc += 1,
                        Flow::Goto(target) => pc = target,
                        Flow::Unwind(value, origin) => {
                            return Ok(FrameCompletion::Unwind(value, origin, pc));
                        }
                    }
                }
                other => {
                    let (call, dst) = self.lower(other, &frame);
                    let result = self.dispatch(&mut frame, call);
                    match self.apply(&mut frame, module, function, pc, dst, result)? {
                        Flow::Next => pc += 1,
                        Flow::Goto(target) => pc = target,
                        Flow::Unwind(value, origin) => {
                            return Ok(FrameCompletion::Unwind(value, origin, pc));
                        }
                    }
                }
            }
        }
    }

    /// Lowers a value/heap/host opcode into the [`HelperCall`] the code
    /// generator would emit plus the destination register (if any). Reading
    /// operand registers here mirrors codegen's register loads.
    fn lower(
        &self,
        instruction: Instruction,
        frame: &NativeFrame<'_>,
    ) -> (HelperCall, Option<u32>) {
        let register = |r: bamts_bytecode::Register| frame.register(r.get());
        match instruction {
            Instruction::LoadConst { dst, constant } => (
                HelperCall::LoadConstant {
                    const_id: constant.get(),
                },
                Some(dst.get()),
            ),
            Instruction::Unary { dst, op, operand } => (
                HelperCall::Unary {
                    op: unary_to_selector(op),
                    operand: register(operand),
                },
                Some(dst.get()),
            ),
            Instruction::Binary {
                dst,
                op,
                left,
                right,
            } => (
                HelperCall::Binary {
                    op: binary_to_selector(op),
                    left: register(left),
                    right: register(right),
                },
                Some(dst.get()),
            ),
            Instruction::CreateObject { dst } => (HelperCall::CreateObject, Some(dst.get())),
            Instruction::CreateArray { dst } => (HelperCall::CreateArray, Some(dst.get())),
            Instruction::CreateClosure {
                dst,
                function,
                captures,
            } => (
                HelperCall::CreateClosure {
                    function_id: function.get(),
                    captures: register(captures),
                },
                Some(dst.get()),
            ),
            Instruction::GetProperty { dst, object, key } => (
                HelperCall::GetProperty {
                    object: register(object),
                    key: register(key),
                },
                Some(dst.get()),
            ),
            Instruction::SetProperty { object, key, value } => (
                HelperCall::SetProperty {
                    object: register(object),
                    key: register(key),
                    value: register(value),
                },
                None,
            ),
            Instruction::DeleteProperty { dst, object, key } => (
                HelperCall::DeleteProperty {
                    object: register(object),
                    key: register(key),
                },
                Some(dst.get()),
            ),
            Instruction::DefineAccessor {
                object,
                key,
                accessor,
                kind,
            } => (
                HelperCall::DefineAccessor {
                    object: register(object),
                    key: register(key),
                    accessor: register(accessor),
                    kind: accessor_to_selector(kind),
                },
                None,
            ),
            Instruction::Call {
                dst,
                callee,
                this_value,
                arguments,
            } => (
                HelperCall::Call {
                    callee: register(callee),
                    this_value: register(this_value),
                    arguments: register(arguments),
                },
                Some(dst.get()),
            ),
            Instruction::Construct {
                dst,
                callee,
                arguments,
            } => (
                HelperCall::Construct {
                    callee: register(callee),
                    arguments: register(arguments),
                },
                Some(dst.get()),
            ),
            Instruction::LoadGlobal { dst, name } => {
                (HelperCall::LoadGlobal { name: name.get() }, Some(dst.get()))
            }
            Instruction::StoreGlobal { name, value } => (
                HelperCall::StoreGlobal {
                    name: name.get(),
                    value: register(value),
                },
                None,
            ),
            Instruction::TypeOfGlobal { dst, name } => (
                HelperCall::TypeOfGlobal { name: name.get() },
                Some(dst.get()),
            ),
            Instruction::LoadThis { dst } => (HelperCall::LoadThis, Some(dst.get())),
            Instruction::LoadArguments { dst } => (HelperCall::LoadArguments, Some(dst.get())),
            Instruction::LoadNewTarget { dst } => (HelperCall::LoadNewTarget, Some(dst.get())),
            Instruction::ArrayPush { array, value } => (
                HelperCall::ArrayPush {
                    array: register(array),
                    value: register(value),
                },
                None,
            ),
            Instruction::ArrayExtend { array, iterable } => (
                HelperCall::ArrayExtend {
                    array: register(array),
                    iterable: register(iterable),
                },
                None,
            ),
            Instruction::ObjectSpread { target, source } => (
                HelperCall::ObjectSpread {
                    target: register(target),
                    source: register(source),
                },
                None,
            ),
            Instruction::SetPrototype { object, prototype } => (
                HelperCall::SetPrototype {
                    object: register(object),
                    prototype: register(prototype),
                },
                None,
            ),
            Instruction::CreatePrivateName { dst, description } => (
                HelperCall::CreatePrivateName {
                    description: description.get(),
                },
                Some(dst.get()),
            ),
            Instruction::CreateRegExp {
                dst,
                pattern,
                flags,
            } => (
                HelperCall::CreateRegExp {
                    pattern: pattern.get(),
                    flags: flags.get(),
                },
                Some(dst.get()),
            ),
            Instruction::GetIterator { dst, src, kind } => (
                HelperCall::GetIterator {
                    src: register(src),
                    kind: iterator_kind_to_selector(kind),
                },
                Some(dst.get()),
            ),
            Instruction::IteratorNext {
                done,
                value,
                iterator,
            } => (
                HelperCall::IteratorNext {
                    iterator: register(iterator),
                    done_reg: done.get(),
                    value_reg: value.get(),
                },
                None,
            ),
            Instruction::Import { dst, specifier } => (
                HelperCall::Import {
                    specifier: specifier.get(),
                },
                Some(dst.get()),
            ),
            Instruction::Export { name, src } => (
                HelperCall::Export {
                    name: name.get(),
                    src: register(src),
                },
                None,
            ),
            // Control-flow opcodes are handled by the driver and never lowered.
            Instruction::Move { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::Return { .. }
            | Instruction::Throw { .. }
            | Instruction::Suspend { .. }
            | Instruction::Halt => {
                unreachable!("control-flow opcode is not lowered to a helper call")
            }
        }
    }

    /// Interprets a [`HelperResult`] against the frame: stores a normal result
    /// into `dst`, searches handlers for a throw, or turns a trap into an error.
    fn apply(
        &self,
        frame: &mut NativeFrame<'_>,
        module: ModuleId,
        function: usize,
        pc: usize,
        dst: Option<u32>,
        result: HelperResult,
    ) -> Result<Flow, RuntimeError> {
        match result.tag {
            CompletionTag::Normal => {
                if let Some(register) = dst {
                    frame.set_register(register, result.value);
                }
                Ok(Flow::Next)
            }
            CompletionTag::Throw => {
                let (value, origin) = match self.pending_throw.take() {
                    Some(pending) => (pending.value, pending.origin),
                    None => (result.value, ThrowOrigin::Bytecode),
                };
                Ok(self.raise(frame, module, function, pc, value, origin))
            }
            CompletionTag::Suspend => {
                // The reference driver drives `Suspend` inline; a helper never
                // returns it. Treat as a malformed completion.
                Err(self.error_at(
                    module,
                    RuntimeErrorKind::InvalidValue {
                        value: result.value,
                    },
                    function,
                    pc,
                ))
            }
            CompletionTag::FatalTrap => {
                if let Some(error) = self.pending_error.take() {
                    return Err(error);
                }
                let kind = self.pending_fatal_kind.take().unwrap_or({
                    RuntimeErrorKind::InvalidValue {
                        value: result.value,
                    }
                });
                Err(self.error_at(module, kind, function, pc))
            }
        }
    }

    /// Searches the current function's handlers covering `pc`. Binds the thrown
    /// value into the handler's catch register and jumps, or signals an unwind.
    fn raise(
        &self,
        frame: &mut NativeFrame<'_>,
        module: ModuleId,
        function: usize,
        pc: usize,
        value: Value,
        origin: ThrowOrigin,
    ) -> Flow {
        match crate::innermost_handler(&self.module(module).functions()[function], pc) {
            Some(handler) => {
                frame.set_register(handler.catch_register.get(), value);
                Flow::Goto(handler.handler.get() as usize)
            }
            None => Flow::Unwind(value, origin),
        }
    }

    // -- Callee invocation ---------------------------------------------------

    /// Invokes a callee value: a runtime closure re-enters native execution; a
    /// host function/foreign value routes to the host. Shared with the
    /// interpreter's classification via [`Machine::callee_kind`].
    fn invoke_callee(
        &self,
        callee: Value,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> InvokeOutcome {
        let kind = self.machine.borrow().callee_kind(callee);
        match kind {
            Ok(CalleeKind::Runtime { target, captures }) => {
                self.invoke_runtime(target, &captures, this, new_target, args)
            }
            Ok(CalleeKind::Builtin { id, bound_this }) => {
                let this = bound_this.unwrap_or(this);
                let result = self
                    .machine
                    .borrow_mut()
                    .call_builtin(id, this, args, false);
                match result {
                    Ok(BuiltinOutcome::Value(value)) => InvokeOutcome::Value(value),
                    Ok(BuiltinOutcome::Call {
                        callee,
                        this_value,
                        argument_start,
                    }) => self.invoke_callee(
                        callee,
                        this_value,
                        &args[argument_start..],
                        Value::UNDEFINED,
                    ),
                    Err(EvalFailure::Throw(origin)) => {
                        InvokeOutcome::Threw(Value::UNDEFINED, origin)
                    }
                    Err(EvalFailure::ThrowValue(value)) => {
                        InvokeOutcome::Threw(value, ThrowOrigin::Bytecode)
                    }
                    Err(EvalFailure::Runtime(kind)) => {
                        self.pending_fatal_kind.set(Some(kind));
                        InvokeOutcome::Fatal
                    }
                }
            }
            Ok(CalleeKind::NotCallable) => InvokeOutcome::Threw(
                Value::UNDEFINED,
                ThrowOrigin::TypeError { operation: "call" },
            ),
            Err(kind) => {
                self.pending_fatal_kind.set(Some(kind));
                InvokeOutcome::Fatal
            }
        }
    }

    /// Re-enters a runtime function with the active recursion and register
    /// ceilings, dispatching to the reference driver or the linked entry table.
    fn invoke_runtime(
        &self,
        target: crate::RuntimeFunction,
        captures: &[Value],
        this: Value,
        new_target: Value,
        args: &[Value],
    ) -> InvokeOutcome {
        let index = target.function.get() as usize;
        if self.activations.borrow().len() >= self.max_call_depth() {
            let limit = self.max_call_depth();
            self.pending_fatal_kind
                .set(Some(RuntimeErrorKind::CallDepthExceeded { limit }));
            return InvokeOutcome::Fatal;
        }
        let register_count =
            self.module(target.module).functions()[index].register_count() as usize;
        if self.live_registers.get().saturating_add(register_count) > self.max_total_registers() {
            let limit = self.max_total_registers();
            self.pending_fatal_kind
                .set(Some(RuntimeErrorKind::RegisterLimitExceeded { limit }));
            return InvokeOutcome::Fatal;
        }
        match self.backend {
            Backend::Reference => {
                match self.execute(
                    target.module,
                    index,
                    this,
                    new_target,
                    args.to_vec(),
                    captures,
                ) {
                    Ok((FrameCompletion::Normal(value), _)) => InvokeOutcome::Value(value),
                    Ok((FrameCompletion::Unwind(value, origin, _), _)) => {
                        InvokeOutcome::Threw(value, origin)
                    }
                    Err(error) => {
                        self.pending_error.set(Some(error));
                        InvokeOutcome::Fatal
                    }
                }
            }
            Backend::Linked => self.invoke_linked(target, captures, this, new_target, args),
        }
    }

    /// Invokes a compiled entry through the borrowed [`NativeEntryTable`],
    /// building a fresh child [`ShadowFrame`] over a driver-local register file.
    fn invoke_linked(
        &self,
        target: crate::RuntimeFunction,
        captures: &[Value],
        this: Value,
        new_target: Value,
        args: &[Value],
    ) -> InvokeOutcome {
        let index = target.function.get() as usize;
        let register_count =
            self.module(target.module).functions()[index].register_count() as usize;
        let mut registers = self.seed_registers(target.module, index, captures, args);
        let length = match u16::try_from(registers.len()) {
            Ok(length) => length,
            Err(_) => {
                let limit = self.max_total_registers();
                self.pending_fatal_kind
                    .set(Some(RuntimeErrorKind::RegisterLimitExceeded { limit }));
                return InvokeOutcome::Fatal;
            }
        };
        self.live_registers
            .set(self.live_registers.get() + register_count);
        self.activations.borrow_mut().push(Activation {
            this_value: this,
            new_target,
            args: args.to_vec(),
            arguments_object: None,
            pending_resume: None,
        });
        let handles = registers.as_mut_ptr();
        let mut shadow = ShadowFrame::new(
            std::ptr::null_mut(),
            0,
            target.module.get(),
            handles,
            length,
        );
        let mut out = Completion::new(Value::UNDEFINED);
        let entries = self.entries;
        // No borrow guard is held across `invoke`: the compiled entry re-enters
        // `dispatch` on this same engine, which takes fresh short-lived borrows.
        let tag = entries.invoke(
            target.module.get(),
            target.function.get(),
            &mut shadow,
            &mut out,
        );
        drop(registers);
        self.activations.borrow_mut().pop();
        self.live_registers
            .set(self.live_registers.get() - register_count);
        match tag {
            Ok(CompletionTag::Normal) => InvokeOutcome::Value(out.value),
            Ok(CompletionTag::Throw) => {
                let origin = self
                    .pending_throw
                    .take()
                    .map_or(ThrowOrigin::Bytecode, |pending| pending.origin);
                InvokeOutcome::Threw(out.value, origin)
            }
            Ok(CompletionTag::Suspend | CompletionTag::FatalTrap) => InvokeOutcome::Fatal,
            Err(error) => {
                self.pending_abi_error.set(Some(error));
                InvokeOutcome::Fatal
            }
        }
    }

    // -- HelperResult constructors -------------------------------------------

    fn fatal(&self, kind: RuntimeErrorKind) -> HelperResult {
        self.pending_fatal_kind.set(Some(kind));
        HelperResult {
            tag: CompletionTag::FatalTrap,
            value: Value::UNDEFINED,
        }
    }

    fn fail(&self, failure: EvalFailure) -> HelperResult {
        match failure {
            EvalFailure::Throw(origin) => {
                self.pending_throw.set(Some(PendingThrow {
                    value: Value::UNDEFINED,
                    origin,
                }));
                HelperResult::throw(Value::UNDEFINED)
            }
            EvalFailure::ThrowValue(value) => {
                self.pending_throw.set(Some(PendingThrow {
                    value,
                    origin: ThrowOrigin::Bytecode,
                }));
                HelperResult::throw(value)
            }
            EvalFailure::Runtime(kind) => self.fatal(kind),
        }
    }

    fn eval_result(&self, result: Result<Value, EvalFailure>) -> HelperResult {
        match result {
            Ok(value) => HelperResult::normal(value),
            Err(failure) => self.fail(failure),
        }
    }

    fn outcome_result(&self, outcome: InvokeOutcome) -> HelperResult {
        match outcome {
            InvokeOutcome::Value(value) => HelperResult::normal(value),
            InvokeOutcome::Threw(value, origin) => {
                self.pending_throw.set(Some(PendingThrow { value, origin }));
                HelperResult::throw(value)
            }
            InvokeOutcome::Fatal => HelperResult {
                tag: CompletionTag::FatalTrap,
                value: Value::UNDEFINED,
            },
        }
    }

    fn validated(&self, value: Value) -> HelperResult {
        HelperResult::normal(value)
    }

    fn allocated(&self, entry: HeapEntry) -> HelperResult {
        let result = self.machine.borrow_mut().allocate(entry);
        match result {
            Ok(value) => HelperResult::normal(value),
            Err(kind) => self.fatal(kind),
        }
    }

    fn constant_text(&self, module: ModuleId, id: u32) -> String {
        self.machine
            .borrow()
            .constant_text(module, ConstantId::new(id))
            .to_owned()
    }

    /// Materializes the `arguments` object for the current activation, caching it.
    fn load_arguments(&self) -> HelperResult {
        let args = match self.activations.borrow().last() {
            Some(activation) => activation.args.clone(),
            None => return HelperResult::normal(Value::UNDEFINED),
        };
        if let Some(existing) = self
            .activations
            .borrow()
            .last()
            .and_then(|activation| activation.arguments_object)
        {
            return HelperResult::normal(existing);
        }
        let prototype = self.machine.borrow().intrinsics.array_prototype;
        let allocated = self.machine.borrow_mut().allocate(HeapEntry::Array {
            elements: args,
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
            length_writable: true,
        });
        let value = match allocated {
            Ok(value) => value,
            Err(kind) => return self.fatal(kind),
        };
        if let Some(activation) = self.activations.borrow_mut().last_mut() {
            activation.arguments_object = Some(value);
        }
        HelperResult::normal(value)
    }

    /// `Construct`: allocate the instance with the constructor's `prototype`,
    /// invoke with `new.target`, and override a non-object return with the
    /// instance — the shared construct semantics, over native activations.
    fn construct(&self, callee: Value, arguments: &[Value]) -> HelperResult {
        let kind = self.machine.borrow().callee_kind(callee);
        match kind {
            Ok(CalleeKind::Builtin { id, .. }) => {
                let result =
                    self.machine
                        .borrow_mut()
                        .call_builtin(id, Value::UNDEFINED, arguments, true);
                match result {
                    Ok(BuiltinOutcome::Value(value)) => HelperResult::normal(value),
                    Ok(BuiltinOutcome::Call { .. }) => {
                        self.pending_throw.set(Some(PendingThrow {
                            value: Value::UNDEFINED,
                            origin: ThrowOrigin::TypeError {
                                operation: "construct",
                            },
                        }));
                        HelperResult::throw(Value::UNDEFINED)
                    }
                    Err(failure) => self.fail(failure),
                }
            }
            Ok(CalleeKind::Runtime { target, captures }) => {
                let prototype = {
                    let machine = self.machine.borrow();
                    match machine.runtime_slot(callee) {
                        Ok(Some(index)) => match machine.own_data_property(index, "prototype") {
                            Some(value) if machine.is_object(value) => Some(value),
                            _ => Some(machine.intrinsics.object_prototype),
                        },
                        _ => {
                            drop(machine);
                            return self.fatal(RuntimeErrorKind::InvalidValue { value: callee });
                        }
                    }
                };
                let instance = {
                    let allocated = self.machine.borrow_mut().allocate(HeapEntry::Object {
                        properties: PropertyMap::default(),
                        prototype,
                        boxed_primitive: None,
                        extensible: true,
                    });
                    match allocated {
                        Ok(value) => value,
                        Err(kind) => return self.fatal(kind),
                    }
                };
                let outcome = self.invoke_runtime(target, &captures, instance, callee, arguments);
                match outcome {
                    InvokeOutcome::Value(returned) => {
                        let is_object = self.machine.borrow().is_object(returned);
                        HelperResult::normal(if is_object { returned } else { instance })
                    }
                    other => self.outcome_result(other),
                }
            }
            Ok(CalleeKind::NotCallable) => {
                self.pending_throw.set(Some(PendingThrow {
                    value: Value::UNDEFINED,
                    origin: ThrowOrigin::TypeError {
                        operation: "construct",
                    },
                }));
                HelperResult::throw(Value::UNDEFINED)
            }
            Err(kind) => self.fatal(kind),
        }
    }

    // -- Linked backend ------------------------------------------------------

    fn run_linked(&mut self) -> Result<ExecutionOutcome, NativeError> {
        self.machine.borrow_mut().instantiate_modules()?;
        let module = self.program.entry();
        let execution = self.evaluate_linked_module(module)?.ok_or_else(|| {
            let function = self.module(module).entry().get() as usize;
            NativeError::Runtime(self.error_at(
                module,
                RuntimeErrorKind::InvalidVerifiedProgram {
                    module,
                    instruction: Instruction::Halt,
                },
                function,
                0,
            ))
        })?;
        Ok(execution.outcome)
    }

    fn evaluate_linked_module(
        &mut self,
        module: ModuleId,
    ) -> Result<Option<Execution>, NativeError> {
        let dependencies = match self.machine.borrow_mut().begin_module_evaluation(module)? {
            crate::ModuleEvaluation::Cycle => return Ok(None),
            crate::ModuleEvaluation::Evaluated(result) => {
                return result.map(Some).map_err(Into::into);
            }
            crate::ModuleEvaluation::Ready(dependencies) => dependencies,
        };
        for dependency in dependencies {
            match self.evaluate_linked_module(dependency) {
                Ok(_) => {}
                Err(NativeError::Runtime(error)) => {
                    let error = self
                        .machine
                        .borrow_mut()
                        .finish_module_evaluation(module, Err(error))
                        .expect_err("dependency failure remains an error");
                    return Err(NativeError::Runtime(error));
                }
                Err(error) => {
                    self.machine.borrow_mut().abort_module_evaluation(module);
                    return Err(error);
                }
            }
        }

        match self.invoke_linked_entry(module) {
            Ok(execution) => self
                .machine
                .borrow_mut()
                .finish_module_evaluation(module, Ok(execution))
                .map(Some)
                .map_err(Into::into),
            Err(NativeError::Runtime(error)) => {
                let error = self
                    .machine
                    .borrow_mut()
                    .finish_module_evaluation(module, Err(error))
                    .expect_err("module failure remains an error");
                Err(NativeError::Runtime(error))
            }
            Err(error) => {
                self.machine.borrow_mut().abort_module_evaluation(module);
                Err(error)
            }
        }
    }

    fn invoke_linked_entry(&mut self, module: ModuleId) -> Result<Execution, NativeError> {
        let function_id = self.module(module).entry();
        let function = function_id.get() as usize;
        let register_count = self.module(module).functions()[function].register_count() as usize;
        if self.max_call_depth() < 1 {
            return Err(NativeError::Runtime(self.error_at(
                module,
                RuntimeErrorKind::CallDepthExceeded {
                    limit: self.max_call_depth(),
                },
                function,
                0,
            )));
        }
        if register_count > self.max_total_registers() {
            return Err(NativeError::Runtime(self.error_at(
                module,
                RuntimeErrorKind::RegisterLimitExceeded {
                    limit: self.max_total_registers(),
                },
                function,
                0,
            )));
        }
        let mut registers = self.seed_registers(module, function, &[], &[]);
        let length = u16::try_from(register_count).map_err(|_| {
            NativeError::Runtime(self.error_at(
                module,
                RuntimeErrorKind::RegisterLimitExceeded {
                    limit: self.max_total_registers(),
                },
                function,
                0,
            ))
        })?;
        self.live_registers.set(register_count);
        self.activations.borrow_mut().push(Activation {
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            pending_resume: None,
        });
        let handles = registers.as_mut_ptr();
        let mut shadow = ShadowFrame::new(std::ptr::null_mut(), 0, module.get(), handles, length);
        let mut out = Completion::new(Value::UNDEFINED);
        let entries = self.entries;
        let tag = with_native_ops(self, || {
            entries.invoke(module.get(), function_id.get(), &mut shadow, &mut out)
        });
        self.activations.borrow_mut().pop();
        self.live_registers.set(0);
        match tag {
            Ok(CompletionTag::Normal) => Ok(Execution {
                outcome: ExecutionOutcome {
                    stdout: self.stdout.borrow().clone(),
                    exit_code: self.exit_code.get(),
                },
                value: out.value,
                link: out.value,
                entry_registers: registers,
            }),
            Ok(CompletionTag::Throw) => {
                let origin = self
                    .pending_throw
                    .take()
                    .map_or(ThrowOrigin::Bytecode, |pending| pending.origin);
                Err(NativeError::Runtime(self.error_at(
                    module,
                    RuntimeErrorKind::UncaughtThrow {
                        value: out.value,
                        origin,
                    },
                    function,
                    shadow.bytecode_pc as usize,
                )))
            }
            Ok(CompletionTag::Suspend | CompletionTag::FatalTrap) => {
                if let Some(error) = self.pending_abi_error.take() {
                    Err(NativeError::Abi(error))
                } else if let Some(error) = self.pending_error.take() {
                    Err(NativeError::Runtime(error))
                } else if let Some(kind) = self.pending_fatal_kind.take() {
                    Err(NativeError::Runtime(self.error_at(
                        module,
                        kind,
                        function,
                        shadow.bytecode_pc as usize,
                    )))
                } else {
                    Err(NativeError::FatalTrap { value: out.value })
                }
            }
            Err(error) => Err(NativeError::Abi(error)),
        }
    }
}

// -- The NativeOps seam ------------------------------------------------------

impl<'m, 'h, H: Host> NativeOps for NativeEngine<'m, 'h, H> {
    fn truthy(&self, _frame: &mut NativeFrame<'_>, value: Value) -> bool {
        self.machine.borrow().truthy(value)
    }

    fn dispatch(&self, frame: &mut NativeFrame<'_>, call: HelperCall) -> HelperResult {
        let module = ModuleId::new(frame.module_id());
        match call {
            HelperCall::LoadConstant { const_id } => {
                let result = self
                    .machine
                    .borrow_mut()
                    .load_constant_value(module, ConstantId::new(const_id));
                match result {
                    Ok(value) => HelperResult::normal(value),
                    Err(kind) => self.fatal(kind),
                }
            }
            HelperCall::Unary { op, operand } => match unary_from_selector(op) {
                Some(op) => {
                    let result = self.machine.borrow_mut().eval_unary(op, operand);
                    self.eval_result(result)
                }
                None => self.fatal(RuntimeErrorKind::InvalidValue {
                    value: Value::UNDEFINED,
                }),
            },
            HelperCall::Binary { op, left, right } => match binary_from_selector(op) {
                Some(op) => {
                    let result = self.machine.borrow_mut().eval_binary(op, left, right);
                    self.eval_result(result)
                }
                None => self.fatal(RuntimeErrorKind::InvalidValue {
                    value: Value::UNDEFINED,
                }),
            },
            HelperCall::CreateObject => {
                let prototype = self.machine.borrow().intrinsics.object_prototype;
                self.allocated(HeapEntry::Object {
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                    boxed_primitive: None,
                    extensible: true,
                })
            }
            HelperCall::CreateArray => {
                let prototype = self.machine.borrow().intrinsics.array_prototype;
                self.allocated(HeapEntry::Array {
                    elements: Vec::new(),
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                    extensible: true,
                    length_writable: true,
                })
            }
            HelperCall::CreateClosure {
                function_id,
                captures,
            } => {
                let function = FunctionId::new(function_id);
                let materialized = self
                    .machine
                    .borrow()
                    .captures_from_array(module, captures, function);
                match materialized {
                    Ok(captures) => {
                        let prototype = self.machine.borrow().intrinsics.function_prototype;
                        self.allocated(HeapEntry::Function {
                            module,
                            function,
                            captures,
                            properties: PropertyMap::default(),
                            prototype: Some(prototype),
                            extensible: true,
                        })
                    }
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::GetProperty { object, key } => {
                let key = {
                    let coerced = self.machine.borrow().to_property_key(key);
                    match coerced {
                        Ok(key) => key,
                        Err(failure) => return self.fail(failure),
                    }
                };
                let outcome = self.machine.borrow_mut().resolve_get(object, &key);
                match outcome {
                    Ok(GetOutcome::Value(value)) => self.validated(value),
                    Ok(GetOutcome::Text(text)) => self.allocated(HeapEntry::String(text)),
                    Ok(GetOutcome::Getter(getter)) => {
                        let outcome = self.invoke_callee(getter, object, &[], Value::UNDEFINED);
                        self.outcome_result(outcome)
                    }
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::SetProperty { object, key, value } => {
                let key = {
                    let coerced = self.machine.borrow().to_property_key(key);
                    match coerced {
                        Ok(key) => key,
                        Err(failure) => return self.fail(failure),
                    }
                };
                let outcome = self.machine.borrow_mut().resolve_set(object, key, value);
                match outcome {
                    Ok(SetOutcome::Done) => HelperResult::normal(Value::UNDEFINED),
                    Ok(SetOutcome::Setter(setter)) => {
                        let outcome =
                            self.invoke_callee(setter, object, &[value], Value::UNDEFINED);
                        match outcome {
                            InvokeOutcome::Value(_) => HelperResult::normal(Value::UNDEFINED),
                            other => self.outcome_result(other),
                        }
                    }
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::DeleteProperty { object, key } => {
                let key = {
                    let coerced = self.machine.borrow().to_property_key(key);
                    match coerced {
                        Ok(key) => key,
                        Err(failure) => return self.fail(failure),
                    }
                };
                let deleted = self.machine.borrow_mut().delete_property(object, &key);
                match deleted {
                    Ok(deleted) => HelperResult::normal(Value::boolean(deleted)),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::DefineAccessor {
                object,
                key,
                accessor,
                kind,
            } => {
                let kind = match accessor_from_selector(kind) {
                    Some(kind) => kind,
                    None => {
                        return self.fatal(RuntimeErrorKind::InvalidValue {
                            value: Value::UNDEFINED,
                        });
                    }
                };
                let key = {
                    let coerced = self.machine.borrow().to_property_key(key);
                    match coerced {
                        Ok(key) => key,
                        Err(failure) => return self.fail(failure),
                    }
                };
                let defined = self
                    .machine
                    .borrow_mut()
                    .define_accessor(object, key, accessor, kind);
                match defined {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::Call {
                callee,
                this_value,
                arguments,
            } => {
                let arguments = {
                    let read = self.machine.borrow().arguments_from_array(arguments);
                    match read {
                        Ok(arguments) => arguments,
                        Err(failure) => return self.fail(failure),
                    }
                };
                let outcome = self.invoke_callee(callee, this_value, &arguments, Value::UNDEFINED);
                self.outcome_result(outcome)
            }
            HelperCall::Construct { callee, arguments } => {
                let arguments = {
                    let read = self.machine.borrow().arguments_from_array(arguments);
                    match read {
                        Ok(arguments) => arguments,
                        Err(failure) => return self.fail(failure),
                    }
                };
                self.construct(callee, &arguments)
            }
            HelperCall::Import { .. } => self.fail(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "import outside an engine-owned module registry",
            })),
            HelperCall::Truthy { value } => {
                HelperResult::normal(Value::boolean(self.machine.borrow().truthy(value)))
            }
            HelperCall::ResumeValue => {
                let resumed = self
                    .activations
                    .borrow_mut()
                    .last_mut()
                    .and_then(|activation| activation.pending_resume.take());
                match resumed {
                    Some(value) => self.validated(value),
                    None => self.fatal(RuntimeErrorKind::InvalidValue {
                        value: Value::UNDEFINED,
                    }),
                }
            }
            HelperCall::LoadGlobal { name } => {
                let resolved = self
                    .machine
                    .borrow()
                    .load_global(module, ConstantId::new(name));
                match resolved {
                    Ok(Some(value)) => self.validated(value),
                    Ok(None) => {
                        self.pending_throw.set(Some(PendingThrow {
                            value: Value::UNDEFINED,
                            origin: ThrowOrigin::ReferenceError {
                                operation: "global is not defined",
                            },
                        }));
                        HelperResult::throw(Value::UNDEFINED)
                    }
                    Err(kind) => self.fatal(kind),
                }
            }
            HelperCall::StoreGlobal { name, value } => {
                let stored =
                    self.machine
                        .borrow_mut()
                        .store_global(module, ConstantId::new(name), value);
                match stored {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::TypeOfGlobal { name } => {
                let resolved = self
                    .machine
                    .borrow()
                    .load_global(module, ConstantId::new(name));
                let text = match resolved {
                    Ok(Some(value)) => self.machine.borrow().type_of(value).to_owned(),
                    Ok(None) => "undefined".to_owned(),
                    Err(kind) => return self.fatal(kind),
                };
                self.allocated(HeapEntry::String(text))
            }
            HelperCall::LoadThis => HelperResult::normal(
                self.activations
                    .borrow()
                    .last()
                    .map_or(Value::UNDEFINED, |activation| activation.this_value),
            ),
            HelperCall::LoadArguments => self.load_arguments(),
            HelperCall::LoadNewTarget => HelperResult::normal(
                self.activations
                    .borrow()
                    .last()
                    .map_or(Value::UNDEFINED, |activation| activation.new_target),
            ),
            HelperCall::ArrayPush { array, value } => {
                let result = self.machine.borrow_mut().array_push(array, value);
                match result {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::ArrayExtend { array, iterable } => {
                let result = self.machine.borrow_mut().array_extend(array, iterable);
                match result {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::ObjectSpread { target, source } => {
                let result = self.machine.borrow_mut().object_spread(target, source);
                match result {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::SetPrototype { object, prototype } => {
                let result = self.machine.borrow_mut().set_prototype(object, prototype);
                match result {
                    Ok(()) => HelperResult::normal(Value::UNDEFINED),
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::CreatePrivateName { description } => {
                let description = self.constant_text(module, description);
                self.allocated(HeapEntry::PrivateName { description })
            }
            HelperCall::CreateRegExp { pattern, flags } => {
                let pattern = self.constant_text(module, pattern);
                let flags = self.constant_text(module, flags);
                self.allocated(HeapEntry::RegExp {
                    pattern,
                    flags,
                    properties: PropertyMap::default(),
                    extensible: true,
                })
            }
            HelperCall::GetIterator { src, kind } => match iterator_kind_from_selector(kind) {
                Some(kind) => {
                    let result = self.machine.borrow_mut().create_iterator(src, kind);
                    self.eval_result(result)
                }
                None => self.fatal(RuntimeErrorKind::InvalidValue {
                    value: Value::UNDEFINED,
                }),
            },
            HelperCall::IteratorNext {
                iterator,
                done_reg,
                value_reg,
            } => {
                let stepped = self.machine.borrow_mut().iterator_next(iterator);
                match stepped {
                    Ok((done, value)) => {
                        let wrote_done = frame.try_set_register(done_reg, Value::boolean(done));
                        let wrote_value = frame.try_set_register(value_reg, value);
                        if wrote_done && wrote_value {
                            HelperResult::normal(Value::UNDEFINED)
                        } else {
                            self.fatal(RuntimeErrorKind::InvalidValue {
                                value: Value::UNDEFINED,
                            })
                        }
                    }
                    Err(failure) => self.fail(failure),
                }
            }
            HelperCall::Export { .. } => self.fail(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "export outside an engine-owned module registry",
            })),
        }
    }
}

/// Runs a linked program's entry through the native engine and returns its
/// observable outcome. This is the runtime side of the AOT `main`: `entries` is
/// a compiled [`NativeEntryTable`] (an AOT `LinkedProgram` or a JIT program),
/// and the returned [`ExecutionOutcome`] carries buffered stdout and the exit
/// code. It is always available and never feature-gated, so a downstream host
/// crate can link against it without enabling the runtime's `aot-main` feature.
///
/// Returns [`NativeError::ProgramMismatch`] before constructing the engine when
/// `entries` was not compiled from this program's exact canonical encoding.
pub fn run_linked_program<H: Host>(
    program: &Program<Verified>,
    entries: &dyn NativeEntryTable,
    host: &mut H,
    limits: &Limits,
) -> Result<ExecutionOutcome, NativeError> {
    let program_bytes = program.encode();
    if program_bytes != entries.program_bytes() {
        return Err(NativeError::ProgramMismatch);
    }
    let mut engine = NativeEngine::build(program, entries, host, limits.clone(), Backend::Linked);
    engine.run_linked()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use bamts_bytecode::{
        BinaryOp, Binding, BindingId, BindingKind, Constant, ConstantId, Edge, EdgeId, EdgeKind,
        EdgeTarget, ExceptionHandler, Export, ExportSource, Function, FunctionFlags, FunctionId,
        Instruction, Module, ModuleId, Pc, Program, ProgramModule, Register, Verified,
    };
    use bamts_native::{AbiError, Completion, CompletionTag, NativeEntryTable, ShadowFrame, Value};

    use crate::{Host, Limits, Machine, RuntimeError, RuntimeErrorKind, ThrowOrigin};

    use super::{
        Backend, InvokeOutcome, NativeEngine, NativeError, PendingThrow, run_linked_program,
    };

    fn reg(raw: u32) -> Register {
        Register::new(raw)
    }

    fn pc(raw: u32) -> Pc {
        Pc::new(raw)
    }

    fn cid(raw: u32) -> bamts_bytecode::ConstantId {
        bamts_bytecode::ConstantId::new(raw)
    }

    fn entry_function(register_count: u32, code: Vec<Instruction>) -> Function {
        Function::new(
            None,
            0,
            0,
            register_count,
            FunctionFlags::default(),
            code,
            Vec::new(),
        )
    }

    fn verified(constants: Vec<Constant>, functions: Vec<Function>) -> Module<Verified> {
        Module::new(constants, functions, FunctionId::new(0))
            .verify()
            .expect("module verifies")
    }

    fn module_function(captures: u32, registers: u32, code: Vec<Instruction>) -> Function {
        Function::new(
            None,
            captures,
            0,
            registers,
            FunctionFlags::default(),
            code,
            Vec::new(),
        )
    }

    fn program_module(
        name: &str,
        mut constants: Vec<Constant>,
        functions: Vec<Function>,
        edges: Vec<Edge>,
        bindings: Vec<Binding>,
        exports: Vec<Export>,
    ) -> ProgramModule<Verified> {
        constants.insert(0, Constant::String(name.to_owned()));
        ProgramModule {
            name: cid(0),
            code: Module::new(constants, functions, FunctionId::new(0))
                .verify()
                .expect("module fixture verifies"),
            edges,
            bindings,
            exports,
        }
    }

    fn linked(modules: Vec<ProgramModule<Verified>>, entry: u32) -> Program<Verified> {
        Program::link(modules, ModuleId::new(entry)).expect("module fixture links")
    }

    fn assert_program_parity(
        program: &Program<Verified>,
    ) -> Result<crate::Execution, RuntimeError> {
        let limits = Limits::default();
        let mut interpreter_host = SilentHost;
        let interpreter = Machine::new(program, &mut interpreter_host, limits.clone()).run();
        let mut native_host = SilentHost;
        let entries = NoEntries;
        let native = NativeEngine::new(program, &entries, &mut native_host, limits).run();
        assert_eq!(interpreter, native);
        native
    }

    /// A dummy entry table for the reference backend, which never invokes it.
    struct NoEntries;

    impl NativeEntryTable for NoEntries {
        fn program_bytes(&self) -> &[u8] {
            &[]
        }

        fn invoke(
            &self,
            module_id: u32,
            function_id: u32,
            _frame: &mut ShadowFrame,
            _out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            Err(AbiError::UnknownFunction {
                module_id,
                function_id,
            })
        }
    }

    #[derive(Default)]
    struct RecordingEntries {
        program_bytes: Vec<u8>,
        invoked: RefCell<Vec<(u32, u32)>>,
    }

    impl NativeEntryTable for RecordingEntries {
        fn program_bytes(&self) -> &[u8] {
            &self.program_bytes
        }

        fn invoke(
            &self,
            module_id: u32,
            function_id: u32,
            _frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            self.invoked.borrow_mut().push((module_id, function_id));
            *out = Completion::new(Value::UNDEFINED);
            Ok(CompletionTag::Normal)
        }
    }

    struct ForeignEntries {
        program_bytes: Vec<u8>,
        invoked: Cell<bool>,
    }

    impl NativeEntryTable for ForeignEntries {
        fn program_bytes(&self) -> &[u8] {
            &self.program_bytes
        }

        fn invoke(
            &self,
            _module_id: u32,
            _function_id: u32,
            _frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            self.invoked.set(true);
            *out = Completion::new(Value::UNDEFINED);
            Ok(CompletionTag::Normal)
        }
    }

    struct SmokeEntries {
        program_bytes: Vec<u8>,
        invoked: Cell<Option<u32>>,
    }

    impl NativeEntryTable for SmokeEntries {
        fn program_bytes(&self) -> &[u8] {
            &self.program_bytes
        }

        fn invoke(
            &self,
            module_id: u32,
            function_id: u32,
            _frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            assert_eq!(module_id, 0);
            self.invoked.set(Some(function_id));
            *out = Completion::new(Value::UNDEFINED);
            Ok(CompletionTag::Normal)
        }
    }

    #[derive(Default)]
    struct FailingEntries {
        program_bytes: Vec<u8>,
    }

    impl NativeEntryTable for FailingEntries {
        fn program_bytes(&self) -> &[u8] {
            &self.program_bytes
        }

        fn invoke(
            &self,
            module_id: u32,
            function_id: u32,
            _frame: &mut ShadowFrame,
            _out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            Err(AbiError::UnknownFunction {
                module_id,
                function_id,
            })
        }
    }

    struct ThrowEntries;

    impl NativeEntryTable for ThrowEntries {
        fn program_bytes(&self) -> &[u8] {
            &[]
        }

        fn invoke(
            &self,
            _module_id: u32,
            _function_id: u32,
            _frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            *out = Completion::new(Value::UNDEFINED);
            Ok(CompletionTag::Throw)
        }
    }

    struct FatalEntries;

    impl NativeEntryTable for FatalEntries {
        fn program_bytes(&self) -> &[u8] {
            &[]
        }

        fn invoke(
            &self,
            _module_id: u32,
            _function_id: u32,
            _frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            *out = Completion::new(Value::UNDEFINED);
            Ok(CompletionTag::FatalTrap)
        }
    }

    #[test]
    fn native_program_keeps_same_name_globals_module_local() {
        let dependency = |name: &str, value: i32| {
            program_module(
                name,
                vec![Constant::String("x".into()), Constant::Int32(value)],
                vec![module_function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(2),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                )],
                Vec::new(),
                vec![Binding {
                    name: cid(1),
                    kind: BindingKind::Hoisted,
                }],
                vec![Export {
                    name: cid(1),
                    source: ExportSource::Local(BindingId::new(0)),
                }],
            )
        };
        let root = program_module(
            "root",
            vec![
                Constant::String("left".into()),
                Constant::String("right".into()),
                Constant::String("x".into()),
                Constant::String("one".into()),
                Constant::String("two".into()),
            ],
            vec![module_function(
                0,
                3,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(1),
                        name: cid(2),
                    },
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Return { value: reg(2) },
                ],
            )],
            vec![
                Edge {
                    specifier: cid(3),
                    target: EdgeTarget::Local(ModuleId::new(0)),
                    kind: EdgeKind::Static,
                },
                Edge {
                    specifier: cid(4),
                    target: EdgeTarget::Local(ModuleId::new(1)),
                    kind: EdgeKind::Static,
                },
            ],
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: cid(3),
                    },
                },
                Binding {
                    name: cid(2),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(1),
                        name: cid(3),
                    },
                },
            ],
            Vec::new(),
        );
        let execution = assert_program_parity(&linked(
            vec![dependency("one", 1), dependency("two", 2), root],
            2,
        ))
        .unwrap();
        assert_eq!(execution.value, Value::int32(3));
    }

    #[test]
    fn native_program_preserves_live_mutation_and_nested_closure_module() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String("x".into()),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::String("set".into()),
            ],
            vec![
                module_function(
                    0,
                    3,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(2),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::CreateArray { dst: reg(1) },
                        Instruction::CreateClosure {
                            dst: reg(2),
                            function: FunctionId::new(1),
                            captures: reg(1),
                        },
                        Instruction::StoreGlobal {
                            name: cid(4),
                            value: reg(2),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                ),
                module_function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(3),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                ),
            ],
            Vec::new(),
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Hoisted,
                },
                Binding {
                    name: cid(4),
                    kind: BindingKind::Hoisted,
                },
            ],
            vec![
                Export {
                    name: cid(1),
                    source: ExportSource::Local(BindingId::new(0)),
                },
                Export {
                    name: cid(4),
                    source: ExportSource::Local(BindingId::new(1)),
                },
            ],
        );
        let root = program_module(
            "root",
            vec![
                Constant::String("set".into()),
                Constant::String("x".into()),
                Constant::String("dependency".into()),
            ],
            vec![module_function(
                0,
                3,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::CreateArray { dst: reg(1) },
                    Instruction::Call {
                        dst: reg(2),
                        callee: reg(0),
                        this_value: reg(1),
                        arguments: reg(1),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(2),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            vec![Edge {
                specifier: cid(3),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: cid(1),
                    },
                },
                Binding {
                    name: cid(2),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: cid(2),
                    },
                },
            ],
            Vec::new(),
        );
        assert_eq!(
            assert_program_parity(&linked(vec![dependency, root], 1))
                .unwrap()
                .value,
            Value::int32(2)
        );
    }

    #[test]
    fn native_program_cycle_observes_temporal_dead_zone() {
        let first = program_module(
            "first",
            vec![
                Constant::String("a".into()),
                Constant::Int32(1),
                Constant::String("second".into()),
            ],
            vec![module_function(
                0,
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            vec![Edge {
                specifier: cid(3),
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Static,
            }],
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Lexical,
            }],
            vec![Export {
                name: cid(1),
                source: ExportSource::Local(BindingId::new(0)),
            }],
        );
        let second = program_module(
            "second",
            vec![
                Constant::String("a".into()),
                Constant::String("first".into()),
            ],
            vec![module_function(
                0,
                1,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            vec![Edge {
                specifier: cid(2),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Imported {
                    edge: EdgeId::new(0),
                    name: cid(1),
                },
            }],
            Vec::new(),
        );
        let error = assert_program_parity(&linked(vec![first, second], 0)).unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::TemporalDeadZone { module, binding }
                if module == ModuleId::new(1) && binding == BindingId::new(0)
        ));
    }

    #[test]
    fn native_program_namespace_reads_shared_export_cell() {
        let dependency = program_module(
            "dependency",
            vec![Constant::String("x".into()), Constant::Int32(7)],
            vec![module_function(
                0,
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            Vec::new(),
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Hoisted,
            }],
            vec![Export {
                name: cid(1),
                source: ExportSource::Local(BindingId::new(0)),
            }],
        );
        let root = program_module(
            "root",
            vec![
                Constant::String("ns".into()),
                Constant::String("x".into()),
                Constant::String("dependency".into()),
            ],
            vec![module_function(
                0,
                3,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(2),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Return { value: reg(2) },
                ],
            )],
            vec![Edge {
                specifier: cid(3),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Namespace {
                    edge: EdgeId::new(0),
                },
            }],
            Vec::new(),
        );
        assert_eq!(
            assert_program_parity(&linked(vec![dependency, root], 1))
                .unwrap()
                .value,
            Value::int32(7)
        );
    }

    #[test]
    fn native_program_evaluates_duplicate_static_dependency_once() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String("count".into()),
                Constant::Int32(0),
                Constant::Int32(1),
            ],
            vec![module_function(
                0,
                2,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::JumpIfFalse {
                        condition: reg(0),
                        target: pc(4),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(3),
                    },
                    Instruction::Jump { target: pc(6) },
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(3),
                    },
                    Instruction::Binary {
                        dst: reg(0),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            Vec::new(),
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Hoisted,
            }],
            vec![Export {
                name: cid(1),
                source: ExportSource::Local(BindingId::new(0)),
            }],
        );
        let root = program_module(
            "root",
            vec![
                Constant::String("count".into()),
                Constant::String("dependency".into()),
                Constant::String("dependency-again".into()),
            ],
            vec![module_function(
                0,
                1,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            vec![
                Edge {
                    specifier: cid(2),
                    target: EdgeTarget::Local(ModuleId::new(0)),
                    kind: EdgeKind::Static,
                },
                Edge {
                    specifier: cid(3),
                    target: EdgeTarget::Local(ModuleId::new(0)),
                    kind: EdgeKind::Static,
                },
            ],
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Imported {
                    edge: EdgeId::new(0),
                    name: cid(1),
                },
            }],
            Vec::new(),
        );
        assert_eq!(
            assert_program_parity(&linked(vec![dependency, root], 1))
                .unwrap()
                .value,
            Value::int32(1)
        );
    }

    #[test]
    fn native_program_memoizes_thrown_object_identity() {
        let module = program_module(
            "throws",
            Vec::new(),
            vec![module_function(
                0,
                1,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::Throw { value: reg(0) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let error = assert_program_parity(&linked(vec![module], 0)).unwrap_err();
        assert!(matches!(error.kind, RuntimeErrorKind::UncaughtThrow { .. }));
    }

    struct SilentHost;
    impl Host for SilentHost {}

    #[test]
    fn native_matches_interpreter_on_arithmetic() {
        let module = verified(
            vec![Constant::Int32(3), Constant::Int32(4)],
            vec![entry_function(
                2,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(1),
                    },
                    Instruction::Binary {
                        dst: reg(0),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
        );
        let value = assert_parity(&module, || SilentHost);
        assert_eq!(value.as_int32(), Some(7));
    }

    #[test]
    fn native_matches_interpreter_on_loop_and_branches() {
        // acc=0; for i in 1..4 { acc += i } -> 6, exercising Binary compare,
        // JumpIfFalse, and Jump through the native driver's own control flow.
        let module = verified(
            vec![Constant::Int32(0), Constant::Int32(1), Constant::Int32(4)],
            vec![entry_function(
                5,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(1),
                    },
                    Instruction::Binary {
                        dst: reg(4),
                        op: BinaryOp::LessThan,
                        left: reg(1),
                        right: reg(2),
                    },
                    Instruction::JumpIfFalse {
                        condition: reg(4),
                        target: pc(9),
                    },
                    Instruction::Binary {
                        dst: reg(0),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Binary {
                        dst: reg(1),
                        op: BinaryOp::Add,
                        left: reg(1),
                        right: reg(3),
                    },
                    Instruction::Jump { target: pc(4) },
                    Instruction::Return { value: reg(0) },
                ],
            )],
        );
        let value = assert_parity(&module, || SilentHost);
        assert_eq!(value.as_int32(), Some(6));
    }

    #[test]
    fn native_matches_interpreter_on_object_property_roundtrip() {
        let module = verified(
            vec![Constant::String("x".to_owned()), Constant::Int32(5)],
            vec![entry_function(
                3,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(1),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(1),
                        value: reg(2),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Return { value: reg(2) },
                ],
            )],
        );
        let value = assert_parity(&module, || SilentHost);
        assert_eq!(value.as_int32(), Some(5));
    }

    #[test]
    fn native_matches_interpreter_on_closure_call() {
        // fn0: build empty captures, close over fn1, call it. fn1: return 42.
        let entry = entry_function(
            3,
            vec![
                Instruction::CreateArray { dst: reg(0) },
                Instruction::CreateClosure {
                    dst: reg(1),
                    function: FunctionId::new(1),
                    captures: reg(0),
                },
                Instruction::CreateArray { dst: reg(0) },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(0),
                },
                Instruction::Call {
                    dst: reg(0),
                    callee: reg(1),
                    this_value: reg(2),
                    arguments: reg(0),
                },
                Instruction::Return { value: reg(0) },
            ],
        );
        let callee = Function::new(
            None,
            0,
            0,
            1,
            FunctionFlags::default(),
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(1),
                },
                Instruction::Return { value: reg(0) },
            ],
            Vec::new(),
        );
        let module = verified(
            vec![Constant::Undefined, Constant::Int32(42)],
            vec![entry, callee],
        );
        let value = assert_parity(&module, || SilentHost);
        assert_eq!(value.as_int32(), Some(42));
    }

    #[test]
    fn native_matches_interpreter_on_throw_and_catch() {
        let module = verified(
            vec![Constant::Int32(99)],
            vec![Function::new(
                None,
                0,
                0,
                2,
                FunctionFlags::default(),
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::Throw { value: reg(0) },
                    Instruction::Return { value: reg(1) },
                ],
                vec![ExceptionHandler {
                    start: pc(0),
                    end: pc(2),
                    handler: pc(2),
                    catch_register: reg(1),
                }],
            )],
        );
        let value = assert_parity(&module, || SilentHost);
        assert_eq!(value.as_int32(), Some(99));
    }

    fn one_module_program(module: &Module<Verified>) -> Program<Verified> {
        let mut constants = module.constants().to_vec();
        let name = ConstantId::new(constants.len() as u32);
        constants.push(Constant::String("test-module".to_owned()));
        let code = Module::new(constants, module.functions().to_vec(), module.entry())
            .verify()
            .expect("test module remains verified");
        Program::link(
            vec![ProgramModule {
                name,
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("one-module reference program links")
    }

    fn assert_parity<H: Host, F: Fn() -> H>(module: &Module<Verified>, make_host: F) -> Value {
        let limits = Limits::default();
        let program = one_module_program(module);

        let mut interp_host = make_host();
        let interpreter = Machine::new(&program, &mut interp_host, limits.clone())
            .run()
            .expect("interpreter runs");

        let mut native_host = make_host();
        let entries = NoEntries;
        let native = NativeEngine::new(&program, &entries, &mut native_host, limits)
            .run()
            .expect("native engine runs");

        assert_eq!(
            interpreter.value, native.value,
            "return value parity: interpreter {:?} vs native {:?}",
            interpreter.value, native.value
        );
        assert_eq!(
            interpreter.outcome, native.outcome,
            "outcome parity: interpreter {:?} vs native {:?}",
            interpreter.outcome, native.outcome
        );
        assert_eq!(
            interpreter.entry_registers, native.entry_registers,
            "entry register parity"
        );
        native.value
    }

    fn trivial_program() -> Program<Verified> {
        let code = verified(
            vec![Constant::String("<test>".to_owned())],
            vec![entry_function(1, vec![Instruction::Halt])],
        );
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("valid native test program")
    }

    #[test]
    fn linked_backend_accepts_metadata_empty_single_module_program() {
        let program = trivial_program();
        let entries = SmokeEntries {
            program_bytes: program.encode(),
            invoked: Cell::new(None),
        };
        let mut host = SilentHost;
        let outcome = run_linked_program(&program, &entries, &mut host, &Limits::default())
            .expect("linked program runs");
        assert_eq!(entries.invoked.get(), Some(0), "entry function 0 invoked");
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn linked_backend_propagates_abi_error() {
        let program = trivial_program();
        let entries = FailingEntries {
            program_bytes: program.encode(),
        };
        let mut host = SilentHost;
        let error = run_linked_program(&program, &entries, &mut host, &Limits::default())
            .expect_err("entry table failure surfaces");
        assert!(matches!(
            error,
            NativeError::Abi(AbiError::UnknownFunction {
                module_id: 0,
                function_id: 0
            })
        ));
    }

    #[test]
    fn linked_backend_rejects_entries_from_same_shape_different_program_before_invocation() {
        let program = |constant| {
            linked(
                vec![program_module(
                    "entry",
                    vec![Constant::Int32(constant)],
                    vec![entry_function(
                        1,
                        vec![
                            Instruction::LoadConst {
                                dst: reg(0),
                                constant: cid(1),
                            },
                            Instruction::Return { value: reg(0) },
                        ],
                    )],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
                0,
            )
        };
        let compiled_program = program(1);
        let supplied_program = program(2);
        let entries = ForeignEntries {
            program_bytes: compiled_program.encode(),
            invoked: Cell::new(false),
        };
        let mut host = SilentHost;

        assert_eq!(
            run_linked_program(&supplied_program, &entries, &mut host, &Limits::default()),
            Err(NativeError::ProgramMismatch)
        );
        assert!(
            !entries.invoked.get(),
            "mismatched entries must not be invoked"
        );
    }

    #[test]
    fn linked_backend_invokes_static_dependencies_before_entry_by_tuple() {
        let single = trivial_program();
        let first = single.modules()[0].clone();
        let second = program_module(
            "entry",
            vec![Constant::String("dependency".into())],
            vec![entry_function(1, vec![Instruction::Halt])],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            Vec::new(),
            Vec::new(),
        );
        let program = Program::link(vec![first, second], ModuleId::new(1)).unwrap();
        let entries = RecordingEntries {
            program_bytes: program.encode(),
            ..RecordingEntries::default()
        };
        let mut host = SilentHost;
        run_linked_program(&program, &entries, &mut host, &Limits::default()).unwrap();
        assert_eq!(entries.invoked.borrow().as_slice(), &[(0, 0), (1, 0)]);
    }

    #[test]
    fn native_functions_call_and_construct_with_engine_parity() {
        let module = verified(
            vec![
                Constant::String("Object".to_owned()),
                Constant::String("prototype".to_owned()),
                Constant::String("toString".to_owned()),
                Constant::String("call".to_owned()),
                Constant::String("[object Object]".to_owned()),
                Constant::Undefined,
            ],
            vec![entry_function(
                8,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
                    },
                    Instruction::CreateArray { dst: reg(1) },
                    Instruction::Construct {
                        dst: reg(2),
                        callee: reg(0),
                        arguments: reg(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(1),
                    },
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(0),
                        key: reg(3),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(2),
                    },
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(4),
                        key: reg(3),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(3),
                    },
                    Instruction::GetProperty {
                        dst: reg(5),
                        object: reg(4),
                        key: reg(3),
                    },
                    Instruction::CreateArray { dst: reg(6) },
                    Instruction::ArrayPush {
                        array: reg(6),
                        value: reg(2),
                    },
                    Instruction::Call {
                        dst: reg(5),
                        callee: reg(5),
                        this_value: reg(4),
                        arguments: reg(6),
                    },
                    Instruction::LoadConst {
                        dst: reg(6),
                        constant: cid(4),
                    },
                    Instruction::Binary {
                        dst: reg(7),
                        op: BinaryOp::StrictEqual,
                        left: reg(5),
                        right: reg(6),
                    },
                    Instruction::Return { value: reg(7) },
                ],
            )],
        );
        assert_eq!(assert_parity(&module, || SilentHost), Value::TRUE);
    }

    #[test]
    fn linked_entry_preserves_pending_throw_origin() {
        let program = trivial_program();
        let mut host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &ThrowEntries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine.pending_throw.set(Some(PendingThrow {
            value: Value::UNDEFINED,
            origin: ThrowOrigin::ReferenceError {
                operation: "fixture",
            },
        }));
        let error = engine.run_linked().unwrap_err();
        assert!(matches!(
            error,
            NativeError::Runtime(RuntimeError {
                kind: RuntimeErrorKind::UncaughtThrow {
                    origin: ThrowOrigin::ReferenceError {
                        operation: "fixture"
                    },
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn linked_entry_sources_pending_runtime_fatal_kind() {
        let program = trivial_program();
        let mut host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &FatalEntries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine
            .pending_fatal_kind
            .set(Some(RuntimeErrorKind::InvalidValue { value: Value::NULL }));
        let error = engine.run_linked().unwrap_err();
        assert!(matches!(
            error,
            NativeError::Runtime(RuntimeError {
                kind: RuntimeErrorKind::InvalidValue { value },
                ..
            }) if value == Value::NULL
        ));
    }

    #[test]
    fn nested_linked_unknown_tuple_remains_abi_error() {
        let module = verified(
            Vec::new(),
            vec![
                entry_function(1, vec![Instruction::Halt]),
                entry_function(1, vec![Instruction::Halt]),
            ],
        );
        let program = one_module_program(&module);
        let entries = FailingEntries::default();
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        let outcome = engine.invoke_runtime(
            crate::RuntimeFunction {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
            },
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
        );
        assert!(matches!(outcome, InvokeOutcome::Fatal));
        assert!(matches!(
            engine.pending_abi_error.take(),
            Some(AbiError::UnknownFunction {
                module_id: 0,
                function_id: 1
            })
        ));
    }

    #[test]
    fn linked_abi_failure_leaves_module_retryable() {
        let program = trivial_program();
        let entries = FailingEntries::default();
        let mut host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine.machine.borrow_mut().instantiate_modules().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                engine.evaluate_linked_module(ModuleId::new(0)),
                Err(NativeError::Abi(AbiError::UnknownFunction {
                    module_id: 0,
                    function_id: 0
                }))
            ));
        }
    }
}
