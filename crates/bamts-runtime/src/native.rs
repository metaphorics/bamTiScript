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

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use bamts_bytecode::{
    ConstantId, EcmaString, FunctionId, Instruction, Module, ModuleId, Program, Verified,
};
pub use bamts_native::AbiError;
use bamts_native::{
    Completion, CompletionTag, HelperCall, HelperResult, NativeEntryTable, NativeFrame, NativeOps,
    ShadowFrame, Value, with_native_ops,
};

use crate::intrinsics::BuiltinOutcome;
use crate::{
    CalleeKind, EvalFailure, Execution, ExecutionOutcome, GeneratorResume, GeneratorStart,
    GeneratorState, GetOutcome, HeapEntry, Host, IteratorNextPrepared, Limits, Machine,
    PropertyMap, RuntimeError, RuntimeErrorKind, SetOutcome, SuspendedActivation, ThrowOrigin,
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

fn iterator_close_mode_to_selector(mode: bamts_bytecode::IteratorCloseMode) -> u32 {
    use bamts_bytecode::IteratorCloseMode;
    match mode {
        IteratorCloseMode::Propagate => 0,
        IteratorCloseMode::PreserveAbrupt => 1,
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
    /// The compiler-free driver; nested calls recurse into [`NativeEngine::execute`].
    Reference,
    /// Compiled entries; nested calls go through the [`NativeEntryTable`].
    Linked,
}

/// A resolved bytecode owner that outlives a temporary machine borrow.
enum CodeRef<'m> {
    Root(&'m Program<Verified>),
    Dynamic(Arc<Program<Verified>>),
}

impl CodeRef<'_> {
    fn code(&self, module: ModuleId) -> &Module<Verified> {
        match self {
            Self::Root(program) => {
                &program
                    .module(module)
                    .expect("verified native module id remains in bounds")
                    .code
            }
            Self::Dynamic(program) => &program.modules()[0].code,
        }
    }
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
    Suspend(Value, u32),
}

#[derive(Clone, Copy)]
enum FrameDrive {
    Ordinary,
    GeneratorStart,
    GeneratorResume { token: u32, sent: Value },
}

/// The result of invoking a callee (runtime function, host entity, or foreign
/// value).
enum InvokeOutcome {
    Value(Value),
    Threw(Value, ThrowOrigin),
    /// An unrecoverable error; the engine's pending trap state is set.
    Fatal,
}

enum ImportFailure {
    Threw(RuntimeError),
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
        let mut machine = Machine::new(program, host, limits);
        machine.frames.clear();
        machine.live_registers = 0;
        NativeEngine {
            machine: RefCell::new(machine),
            program,
            entries,
            backend,
            activations: RefCell::new(Vec::new()),
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
        debug_assert!((module.get() as usize) < self.machine.borrow().dynamic_base);
        &self
            .program
            .module(module)
            .expect("verified native module id remains in bounds")
            .code
    }

    fn code_ref(&self, module: ModuleId) -> CodeRef<'m> {
        let dynamic = {
            let machine = self.machine.borrow();
            let index = module.get() as usize;
            (index >= machine.dynamic_base).then(|| {
                machine.dynamic[index - machine.dynamic_base]
                    .program
                    .clone()
            })
        };
        dynamic.map_or(CodeRef::Root(self.program), CodeRef::Dynamic)
    }

    fn is_dynamic_module(&self, module: ModuleId) -> bool {
        module.get() as usize >= self.machine.borrow().dynamic_base
    }

    /// Builds the root set for the current native activation. The registers are
    /// driver-stack locals, so they must be copied into the Machine before GC.
    fn native_root_snapshot(&self, registers: &[Value]) -> Vec<Value> {
        let activations = self.activations.borrow();
        let activation = activations
            .last()
            .expect("a native root snapshot requires an active activation");
        let mut roots = Vec::with_capacity(
            registers.len()
                + activation.args.len()
                + 2
                + usize::from(activation.arguments_object.is_some())
                + usize::from(activation.pending_resume.is_some())
                + usize::from(self.pending_throw.get().is_some()),
        );
        roots.extend_from_slice(registers);
        roots.push(activation.this_value);
        roots.push(activation.new_target);
        roots.extend_from_slice(&activation.args);
        roots.extend(activation.arguments_object);
        roots.extend(activation.pending_resume);
        roots.extend(self.pending_throw.get().map(|pending| pending.value));
        roots
    }

    fn push_native_roots(&self, registers: &[Value]) {
        let depth = self.activations.borrow().len() - 1;
        let roots = self.native_root_snapshot(registers);
        self.machine.borrow_mut().push_native_roots(depth, &roots);
    }

    fn refresh_native_roots(&self, frame: &NativeFrame<'_>) {
        let depth = self.activations.borrow().len() - 1;
        let roots = self.native_root_snapshot(frame.registers());
        self.machine
            .borrow_mut()
            .refresh_native_roots(depth, &roots);
    }

    fn pop_native_roots(&self) {
        let depth = self.activations.borrow().len() - 1;
        self.machine.borrow_mut().pop_native_roots(depth);
    }

    /// The only re-entrant native-helper boundary. Refresh first because the
    /// helper may collect before it decodes operands or enters another engine.
    fn prepare_native_helper(&self, frame: &NativeFrame<'_>) {
        self.refresh_native_roots(frame);
        let mut machine = self.machine.borrow_mut();
        if machine.gc_pending() {
            machine.collect_if_pending();
        }
    }

    // -- Reference backend: control-flow driver ------------------------------

    /// Executes the program entry with the reference driver, then drives the
    /// shared automatic event loop to quiescence. Never invokes [`Machine::run`];
    /// it reuses the machine's own loop driver directly.
    pub fn run(self) -> Result<Execution, RuntimeError> {
        self.machine.borrow_mut().instantiate_modules()?;
        let entry = self.program.entry();
        let execution = if self.machine.borrow().module_graph_suspends(entry) {
            self.machine
                .borrow_mut()
                .evaluate_instantiated_module(entry)?
        } else {
            self.evaluate_reference_module(entry)?.ok_or_else(|| {
                let function = self.module(entry).entry().get() as usize;
                self.error_at(
                    entry,
                    RuntimeErrorKind::InvalidVerifiedProgram {
                        module: entry,
                        instruction: Instruction::Halt,
                    },
                    function,
                    0,
                )
            })?
        };
        // After successful evaluation, drain microtasks and timers
        // to quiescence on the single borrowed machine. The guard spans only this
        // statement: the loop runs through the machine's own &mut methods, so no
        // nested RefCell borrow is taken and no native callback overlaps it.
        self.machine.borrow_mut().run_to_quiescence()?;
        Ok(execution)
    }

    fn evaluate_reference_module(
        &self,
        module: ModuleId,
    ) -> Result<Option<Execution>, RuntimeError> {
        let dependencies = match self.machine.borrow_mut().begin_module_evaluation(module)? {
            crate::ModuleEvaluation::Cycle => return Ok(None),
            crate::ModuleEvaluation::Evaluated(result) => return result.map(|()| None),
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
                FrameCompletion::Suspend(value, _) => Err(self.error_at(
                    module,
                    RuntimeErrorKind::InvalidValue { value },
                    function,
                    0,
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
        code: &Module<Verified>,
        function: usize,
        captures: &[Value],
        args: &[Value],
    ) -> Vec<Value> {
        let metadata = &code.functions()[function];
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
        let handle = self.code_ref(module);
        let code = handle.code(module);
        let register_count = code.functions()[function].register_count() as usize;
        let mut registers = self.seed_registers(code, function, captures, &args);
        let reserved = self
            .machine
            .borrow_mut()
            .reserve_native_activation(register_count);
        reserved.map_err(|kind| self.error_at(module, kind, function, 0))?;
        self.activations.borrow_mut().push(Activation {
            this_value,
            new_target,
            args,
            arguments_object: None,
            pending_resume: None,
        });
        self.push_native_roots(&registers);
        let completion =
            self.run_frame(module, function, code, &mut registers, FrameDrive::Ordinary);
        self.pop_native_roots();
        self.activations.borrow_mut().pop();
        self.machine
            .borrow_mut()
            .release_native_activation(register_count);
        completion.map(|completion| (completion, registers))
    }

    /// The instruction loop for one activation. `registers` is the caller-owned
    /// register file; a [`NativeFrame`] borrows it disjointly from `self`.
    fn run_frame(
        &self,
        module: ModuleId,
        function: usize,
        code: &Module<Verified>,
        registers: &mut Vec<Value>,
        drive: FrameDrive,
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

        let target = crate::RuntimeFunction {
            module,
            function: FunctionId::new(function as u32),
        };
        let mut pc = match drive {
            FrameDrive::Ordinary | FrameDrive::GeneratorStart => 0,
            FrameDrive::GeneratorResume { token, sent } => {
                let suspend_pc = token.checked_sub(1).map(|pc| pc as usize).ok_or_else(|| {
                    self.error_at(
                        module,
                        RuntimeErrorKind::InvalidValue {
                            value: Value::UNDEFINED,
                        },
                        function,
                        0,
                    )
                })?;
                let Instruction::Suspend { dst, resume, .. } =
                    code.functions()[function].code()[suspend_pc]
                else {
                    return Err(self.error_at(
                        module,
                        RuntimeErrorKind::InvalidValue {
                            value: Value::UNDEFINED,
                        },
                        function,
                        suspend_pc,
                    ));
                };
                frame.set_register(dst.get(), sent);
                resume.get() as usize
            }
        };
        loop {
            let instruction = code.functions()[function].code()[pc];
            if is_inline_instruction(instruction) {
                let consumed = self.machine.borrow_mut().consume_fuel(1);
                if let Err(kind) = consumed {
                    return Err(self.error_at(module, kind, function, pc));
                }
            }

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
                        code,
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
                Instruction::Suspend { src, .. }
                    if matches!(
                        drive,
                        FrameDrive::GeneratorStart | FrameDrive::GeneratorResume { .. }
                    ) =>
                {
                    let token = u32::try_from(pc + 1).map_err(|_| {
                        self.error_at(
                            module,
                            RuntimeErrorKind::InvalidValue {
                                value: Value::UNDEFINED,
                            },
                            function,
                            pc,
                        )
                    })?;
                    return Ok(FrameCompletion::Suspend(frame.register(src.get()), token));
                }
                Instruction::Suspend { .. } | Instruction::Await { .. } => {
                    match self.raise(
                        &mut frame,
                        code,
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
                    match self.apply(&mut frame, target, code, pc, dst, result)? {
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
            Instruction::CreateCell { dst } => (HelperCall::CreateCell, Some(dst.get())),
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
            Instruction::IteratorStep { dst, iterator } => (
                HelperCall::IteratorStep {
                    iterator: register(iterator),
                },
                Some(dst.get()),
            ),
            Instruction::IteratorResult {
                done,
                value,
                result,
            } => (
                HelperCall::IteratorResult {
                    result: register(result),
                    done_reg: done.get(),
                    value_reg: value.get(),
                },
                None,
            ),
            Instruction::IteratorClose {
                result,
                called,
                iterator,
                mode,
            } => (
                HelperCall::IteratorClose {
                    iterator: register(iterator),
                    mode: iterator_close_mode_to_selector(mode),
                    called_reg: called.get(),
                },
                Some(result.get()),
            ),
            Instruction::RequireCloseResult { result, called } => (
                HelperCall::RequireCloseResult {
                    result: register(result),
                    called: register(called),
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
            | Instruction::Await { .. }
            | Instruction::Halt => {
                unreachable!("control-flow opcode is not lowered to a helper call")
            }
        }
    }

    /// Interprets a [`HelperResult`] against the frame: stores a normal result
    /// into `dst`, searches handlers for a throw, or turns a trap into an error.
    fn take_matching_throw(&self, value: Value) -> (Value, ThrowOrigin) {
        match self.pending_throw.take() {
            Some(pending) if pending.value == value => (pending.value, pending.origin),
            Some(_) | None => (value, ThrowOrigin::Bytecode),
        }
    }

    fn apply(
        &self,
        frame: &mut NativeFrame<'_>,
        target: crate::RuntimeFunction,
        code: &Module<Verified>,
        pc: usize,
        dst: Option<u32>,
        result: HelperResult,
    ) -> Result<Flow, RuntimeError> {
        let function = target.function.get() as usize;
        match result.tag {
            CompletionTag::Normal => {
                self.pending_throw.take();
                if let Some(register) = dst {
                    frame.set_register(register, result.value);
                }
                Ok(Flow::Next)
            }
            CompletionTag::Throw => {
                let (value, origin) = self.take_matching_throw(result.value);
                Ok(self.raise(frame, code, function, pc, value, origin))
            }
            CompletionTag::Suspend => {
                // The reference driver drives `Suspend` inline; a helper never
                // returns it. Treat as a malformed completion.
                Err(self.error_at(
                    target.module,
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
                Err(self.error_at(target.module, kind, function, pc))
            }
        }
    }

    /// Searches the current function's handlers covering `pc`. Binds the thrown
    /// value into the handler's catch register and jumps, or signals an unwind.
    fn raise(
        &self,
        frame: &mut NativeFrame<'_>,
        code: &Module<Verified>,
        function: usize,
        pc: usize,
        value: Value,
        origin: ThrowOrigin,
    ) -> Flow {
        match crate::innermost_handler(&code.functions()[function], pc) {
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
        let mut callee = callee;
        let mut this = this;
        let mut args = Cow::Borrowed(args);
        let mut new_target = new_target;
        loop {
            let kind = self.machine.borrow().callee_kind(callee);
            match kind {
                Ok(CalleeKind::Runtime { target, captures }) => {
                    return self.invoke_runtime(target, &captures, this, new_target, args.as_ref());
                }
                Ok(CalleeKind::Builtin { id }) => {
                    let result = {
                        self.machine
                            .borrow_mut()
                            .call_builtin(id, this, args.as_ref(), false)
                    };
                    match result {
                        Ok(BuiltinOutcome::Value(value)) => {
                            return InvokeOutcome::Value(value);
                        }
                        Ok(BuiltinOutcome::Call {
                            callee: next,
                            this_value: next_this,
                            arguments: next_arguments,
                        }) => {
                            callee = next;
                            this = next_this;
                            args = Cow::Owned(next_arguments);
                            new_target = Value::UNDEFINED;
                        }
                        Ok(BuiltinOutcome::GeneratorNext {
                            generator,
                            resume_value,
                        }) => {
                            return self.resume_generator(generator, resume_value);
                        }
                        Ok(BuiltinOutcome::AsyncGeneratorNext {
                            generator,
                            resume_value,
                        }) => {
                            let outcome = self
                                .machine
                                .borrow_mut()
                                .enqueue_async_generator_next(generator, resume_value);
                            return match outcome {
                                Ok(promise) => InvokeOutcome::Value(promise),
                                Err(failure) => self.failure_outcome(failure),
                            };
                        }
                        Err(EvalFailure::Throw(origin)) => {
                            return InvokeOutcome::Threw(Value::UNDEFINED, origin);
                        }
                        Err(EvalFailure::ThrowValue(value)) => {
                            return InvokeOutcome::Threw(value, ThrowOrigin::Bytecode);
                        }
                        Err(EvalFailure::ThrowValueOrigin { value, origin }) => {
                            return InvokeOutcome::Threw(value, origin);
                        }
                        Err(EvalFailure::Runtime(kind)) => {
                            self.pending_fatal_kind.set(Some(kind));
                            return InvokeOutcome::Fatal;
                        }
                    }
                }
                Ok(CalleeKind::Bound) => {
                    let bound = self
                        .machine
                        .borrow()
                        .flatten_bound(callee, this, args.as_ref());
                    match bound {
                        Ok(bound) => {
                            callee = bound.target;
                            if new_target == Value::UNDEFINED {
                                this = bound.this_value;
                            }
                            args = Cow::Owned(bound.arguments);
                        }
                        Err(kind) => {
                            self.pending_fatal_kind.set(Some(kind));
                            return InvokeOutcome::Fatal;
                        }
                    }
                }
                Ok(CalleeKind::NotCallable) => {
                    return InvokeOutcome::Threw(
                        Value::UNDEFINED,
                        ThrowOrigin::TypeError { operation: "call" },
                    );
                }
                Err(kind) => {
                    self.pending_fatal_kind.set(Some(kind));
                    return InvokeOutcome::Fatal;
                }
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
        let handle = self.code_ref(target.module);
        let flags = handle.code(target.module).functions()[index].flags();
        if flags.is_async && flags.is_generator {
            let created = self
                .machine
                .borrow_mut()
                .create_async_generator(GeneratorStart {
                    target,
                    captures: captures.to_vec(),
                    this_value: this,
                    new_target,
                    args: args.to_vec(),
                });
            return match created {
                Ok(generator) => InvokeOutcome::Value(generator),
                Err(kind) => {
                    self.pending_fatal_kind.set(Some(kind));
                    InvokeOutcome::Fatal
                }
            };
        }
        if flags.is_generator && !flags.is_async {
            let created = self.machine.borrow_mut().create_generator(GeneratorStart {
                target,
                captures: captures.to_vec(),
                this_value: this,
                new_target,
                args: args.to_vec(),
            });
            return match created {
                Ok(generator) => InvokeOutcome::Value(generator),
                Err(kind) => {
                    self.pending_fatal_kind.set(Some(kind));
                    InvokeOutcome::Fatal
                }
            };
        }
        if flags.is_async && !flags.is_generator {
            // Async calls route through the shared reference Machine state
            // machine, which drives the body on the interpreter to its first
            // await or completion and returns the implicit Promise. No
            // bytecode, codegen, or ABI change is involved.
            let outcome = self
                .machine
                .borrow_mut()
                .start_async_call(target, captures, this, new_target, args);
            return match outcome {
                Ok(promise) => InvokeOutcome::Value(promise),
                Err(failure) => self.failure_outcome(failure),
            };
        }
        if self.backend == Backend::Reference || self.is_dynamic_module(target.module) {
            return match self.execute(
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
                Ok((FrameCompletion::Suspend(value, _), _)) => {
                    self.pending_fatal_kind
                        .set(Some(RuntimeErrorKind::InvalidValue { value }));
                    InvokeOutcome::Fatal
                }
                Err(error) => {
                    self.pending_error.set(Some(error));
                    InvokeOutcome::Fatal
                }
            };
        }
        self.invoke_linked(target, captures, this, new_target, args)
    }

    fn resume_generator(&self, generator: Value, resume_value: Value) -> InvokeOutcome {
        let state = self.machine.borrow_mut().take_generator_state(generator);
        let resumed = match state {
            Ok(GeneratorState::Completed) => {
                return self.generator_result(Value::UNDEFINED, true);
            }
            Ok(GeneratorState::SuspendedStart(start)) => {
                if self.backend == Backend::Reference || self.is_dynamic_module(start.target.module)
                {
                    self.start_reference_generator(start)
                } else {
                    self.start_linked_generator(start)
                }
            }
            Ok(GeneratorState::Suspended(activation)) => {
                if self.backend == Backend::Reference
                    || self.is_dynamic_module(activation.target.module)
                {
                    self.resume_reference_generator(activation, resume_value)
                } else {
                    self.resume_linked_generator(activation, resume_value)
                }
            }
            Ok(GeneratorState::Executing) => {
                unreachable!("executing state is rejected by take_generator_state")
            }
            Err(failure) => return self.failure_outcome(failure),
        };

        match resumed {
            Some(GeneratorResume::Yield { value, activation }) => {
                let result = self
                    .machine
                    .borrow_mut()
                    .settle_generator_yield(generator, value, activation);
                self.eval_outcome(result)
            }
            Some(GeneratorResume::Return(value)) => {
                let settled = self
                    .machine
                    .borrow_mut()
                    .settle_generator_completed(generator);
                if let Err(failure) = settled {
                    return self.failure_outcome(failure);
                }
                self.generator_result(value, true)
            }
            Some(GeneratorResume::Throw { value, origin, .. }) => {
                let settled = self
                    .machine
                    .borrow_mut()
                    .settle_generator_completed(generator);
                if let Err(failure) = settled {
                    return self.failure_outcome(failure);
                }
                InvokeOutcome::Threw(value, origin)
            }
            None => {
                if let Err(failure) = self
                    .machine
                    .borrow_mut()
                    .settle_generator_completed(generator)
                {
                    return self.failure_outcome(failure);
                }
                InvokeOutcome::Fatal
            }
        }
    }

    fn generator_result(&self, value: Value, done: bool) -> InvokeOutcome {
        let result = self.machine.borrow_mut().iterator_result(value, done);
        self.eval_outcome(result)
    }

    fn eval_outcome(&self, result: Result<Value, EvalFailure>) -> InvokeOutcome {
        match result {
            Ok(value) => InvokeOutcome::Value(value),
            Err(failure) => self.failure_outcome(failure),
        }
    }

    fn failure_outcome(&self, failure: EvalFailure) -> InvokeOutcome {
        match failure {
            EvalFailure::Throw(origin) => InvokeOutcome::Threw(Value::UNDEFINED, origin),
            EvalFailure::ThrowValue(value) => InvokeOutcome::Threw(value, ThrowOrigin::Bytecode),
            EvalFailure::ThrowValueOrigin { value, origin } => InvokeOutcome::Threw(value, origin),
            EvalFailure::Runtime(kind) => {
                self.pending_fatal_kind.set(Some(kind));
                InvokeOutcome::Fatal
            }
        }
    }

    fn get_outcome(
        &self,
        outcome: Result<GetOutcome, EvalFailure>,
        receiver: Value,
    ) -> InvokeOutcome {
        match outcome {
            Ok(GetOutcome::Value(value)) => InvokeOutcome::Value(value),
            Ok(GetOutcome::Text(text)) => {
                match self.machine.borrow_mut().allocate(HeapEntry::String(text)) {
                    Ok(value) => InvokeOutcome::Value(value),
                    Err(kind) => {
                        self.pending_fatal_kind.set(Some(kind));
                        InvokeOutcome::Fatal
                    }
                }
            }
            Ok(GetOutcome::Getter(getter)) => {
                self.invoke_callee(getter, receiver, &[], Value::UNDEFINED)
            }
            Err(failure) => self.failure_outcome(failure),
        }
    }

    fn get_ascii(&self, object: Value, name: &str) -> InvokeOutcome {
        let outcome = self.machine.borrow_mut().resolve_get_ascii(object, name);
        self.get_outcome(outcome, object)
    }

    fn get_iterator_active(
        &self,
        source: Value,
        kind: bamts_bytecode::IteratorKind,
    ) -> InvokeOutcome {
        if kind == bamts_bytecode::IteratorKind::Keys {
            let created = self.machine.borrow_mut().create_iterator(source, kind);
            return self.eval_outcome(created);
        }
        let symbol = match kind {
            bamts_bytecode::IteratorKind::Async => self
                .machine
                .borrow()
                .intrinsics
                .builtins
                .symbol_async_iterator(),
            bamts_bytecode::IteratorKind::Sync => {
                self.machine.borrow().intrinsics.builtins.symbol_iterator()
            }
            bamts_bytecode::IteratorKind::Keys => unreachable!("keys handled above"),
        };
        let key = match self.machine.borrow_mut().to_property_key(symbol) {
            Ok(key) => key,
            Err(failure) => return self.failure_outcome(failure),
        };
        let method = self.machine.borrow_mut().resolve_get(source, &key);
        let mut method = match self.get_outcome(method, source) {
            InvokeOutcome::Value(method) => method,
            other => return other,
        };
        let mut from_sync = false;
        if kind == bamts_bytecode::IteratorKind::Async
            && matches!(method, Value::UNDEFINED | Value::NULL)
        {
            from_sync = true;
            let symbol = self.machine.borrow().intrinsics.builtins.symbol_iterator();
            let key = match self.machine.borrow_mut().to_property_key(symbol) {
                Ok(key) => key,
                Err(failure) => return self.failure_outcome(failure),
            };
            let fallback = self.machine.borrow_mut().resolve_get(source, &key);
            method = match self.get_outcome(fallback, source) {
                InvokeOutcome::Value(method) => method,
                other => return other,
            };
        }
        match self.machine.borrow().is_callable(method) {
            Ok(true) => {}
            Ok(false) => {
                return InvokeOutcome::Threw(
                    Value::UNDEFINED,
                    ThrowOrigin::TypeError {
                        operation: "value is not iterable",
                    },
                );
            }
            Err(failure) => return self.failure_outcome(failure),
        }
        let iterator = match self.invoke_callee(method, source, &[], Value::UNDEFINED) {
            InvokeOutcome::Value(iterator) => iterator,
            other => return other,
        };
        if !self.machine.borrow().is_object(iterator) {
            return InvokeOutcome::Threw(
                Value::UNDEFINED,
                ThrowOrigin::TypeError {
                    operation: "iterator method returned a non-object",
                },
            );
        }
        let next = match self.get_ascii(iterator, "next") {
            InvokeOutcome::Value(next) => next,
            other => return other,
        };
        let created = if from_sync {
            self.machine
                .borrow_mut()
                .create_async_from_sync_iterator(iterator, next)
        } else {
            self.machine
                .borrow_mut()
                .create_protocol_iterator(iterator, next)
        };
        self.eval_outcome(created)
    }

    fn iterator_step_active(&self, iterator: Value) -> Result<Value, InvokeOutcome> {
        let prepared = self.machine.borrow_mut().prepare_iterator_next(iterator);
        match prepared {
            Ok(IteratorNextPrepared::Ready { done, value }) => {
                let result = self.machine.borrow_mut().iterator_result(value, done);
                result.map_err(|failure| self.failure_outcome(failure))
            }
            Ok(IteratorNextPrepared::Call { callee, this_value }) => {
                match self.invoke_callee(callee, this_value, &[], Value::UNDEFINED) {
                    InvokeOutcome::Value(result) => Ok(result),
                    other => Err(other),
                }
            }
            Ok(IteratorNextPrepared::AsyncFromSyncCall { callee, this_value }) => {
                let call = match self.invoke_callee(callee, this_value, &[], Value::UNDEFINED) {
                    InvokeOutcome::Value(result) => Ok(result),
                    InvokeOutcome::Threw(value, origin) => {
                        Err(EvalFailure::ThrowValueOrigin { value, origin })
                    }
                    InvokeOutcome::Fatal => return Err(InvokeOutcome::Fatal),
                };
                self.machine
                    .borrow_mut()
                    .async_from_sync_continuation(this_value, call)
                    .map_err(|failure| self.failure_outcome(failure))
            }
            Err(failure) => Err(self.failure_outcome(failure)),
        }
    }

    fn close_iterator_active(
        &self,
        frame: &mut NativeFrame<'_>,
        iterator: Value,
        mode: u32,
        called_reg: u32,
    ) -> Result<Value, InvokeOutcome> {
        frame.set_register(called_reg, Value::FALSE);
        let preserve_abrupt = mode == 1;
        let map_outcome = |outcome| match outcome {
            InvokeOutcome::Value(value) => Ok(value),
            InvokeOutcome::Fatal => Err(InvokeOutcome::Fatal),
            InvokeOutcome::Threw(_, _) if preserve_abrupt => Ok(Value::UNDEFINED),
            InvokeOutcome::Threw(value, origin) => Err(InvokeOutcome::Threw(value, origin)),
        };
        let target = match self.machine.borrow_mut().iterator_close_target(iterator) {
            Ok(Some(target)) => target,
            Ok(None) => return Ok(Value::UNDEFINED),
            Err(failure) => return map_outcome(self.failure_outcome(failure)),
        };
        let close = match self.get_ascii(target, "return") {
            InvokeOutcome::Value(close) => close,
            outcome => return map_outcome(outcome),
        };
        let callable = match self.machine.borrow().is_callable(close) {
            Ok(callable) => callable,
            Err(failure) => return map_outcome(self.failure_outcome(failure)),
        };
        if !callable {
            return Ok(Value::UNDEFINED);
        }
        frame.set_register(called_reg, Value::TRUE);
        map_outcome(self.invoke_callee(close, target, &[], Value::UNDEFINED))
    }

    fn iterator_result_active(&self, result: Value) -> Result<(bool, Value), InvokeOutcome> {
        if !self.machine.borrow().is_object(result) {
            return Err(InvokeOutcome::Threw(
                Value::UNDEFINED,
                ThrowOrigin::TypeError {
                    operation: "iterator result is not an object",
                },
            ));
        }
        let done = match self.get_ascii(result, "done") {
            InvokeOutcome::Value(done) => done,
            other => return Err(other),
        };
        if self.machine.borrow().truthy(done) {
            return Ok((true, Value::UNDEFINED));
        }
        let value = match self.get_ascii(result, "value") {
            InvokeOutcome::Value(value) => value,
            other => return Err(other),
        };
        Ok((false, value))
    }

    fn iterator_next_active(&self, iterator: Value) -> Result<(bool, Value), InvokeOutcome> {
        // Fused sync consumption reads the raw result directly; the
        // async-from-sync adapter wraps only the `for await` step path.
        let prepared = self.machine.borrow_mut().prepare_iterator_next(iterator);
        let result = match prepared {
            Ok(IteratorNextPrepared::Ready { done, value }) => self
                .machine
                .borrow_mut()
                .iterator_result(value, done)
                .map_err(|failure| self.failure_outcome(failure))?,
            Ok(IteratorNextPrepared::Call { callee, this_value })
            | Ok(IteratorNextPrepared::AsyncFromSyncCall { callee, this_value }) => {
                match self.invoke_callee(callee, this_value, &[], Value::UNDEFINED) {
                    InvokeOutcome::Value(result) => result,
                    other => return Err(other),
                }
            }
            Err(failure) => return Err(self.failure_outcome(failure)),
        };
        self.iterator_result_active(result)
    }

    fn array_extend_active(&self, array: Value, iterable: Value) -> InvokeOutcome {
        let iterator = match self.get_iterator_active(iterable, bamts_bytecode::IteratorKind::Sync)
        {
            InvokeOutcome::Value(iterator) => iterator,
            other => return other,
        };
        loop {
            let (done, value) = match self.iterator_next_active(iterator) {
                Ok(step) => step,
                Err(outcome) => return outcome,
            };
            if done {
                return InvokeOutcome::Value(Value::UNDEFINED);
            }
            if let Err(failure) = self.machine.borrow_mut().array_push(array, value) {
                return self.failure_outcome(failure);
            }
        }
    }

    fn start_reference_generator(&self, start: GeneratorStart) -> Option<GeneratorResume> {
        let target = start.target;
        let index = target.function.get() as usize;
        let handle = self.code_ref(target.module);
        let code = handle.code(target.module);
        let register_count = code.functions()[index].register_count() as usize;
        let mut registers = self.seed_registers(code, index, &start.captures, &start.args);
        if let Err(kind) = self
            .machine
            .borrow_mut()
            .reserve_suspended_activation_registers(register_count)
        {
            self.pending_fatal_kind.set(Some(kind));
            return None;
        }
        if let Err(kind) = self.machine.borrow_mut().enter_native_generator() {
            self.machine
                .borrow_mut()
                .release_suspended_activation_registers(register_count);
            self.pending_fatal_kind.set(Some(kind));
            return None;
        }
        self.activations.borrow_mut().push(Activation {
            this_value: start.this_value,
            new_target: start.new_target,
            args: start.args.clone(),
            arguments_object: None,
            pending_resume: None,
        });
        self.push_native_roots(&registers);
        let completion = self.run_frame(
            target.module,
            index,
            code,
            &mut registers,
            FrameDrive::GeneratorStart,
        );
        self.pop_native_roots();
        let activation = self
            .activations
            .borrow_mut()
            .pop()
            .expect("generator activation exists");
        self.machine.borrow_mut().leave_native_generator();
        self.finish_reference_generator(target, registers, activation, completion)
    }

    fn resume_reference_generator(
        &self,
        mut suspended: SuspendedActivation,
        sent: Value,
    ) -> Option<GeneratorResume> {
        let target = suspended.target;
        let index = target.function.get() as usize;
        let handle = self.code_ref(target.module);
        let code = handle.code(target.module);
        let register_count = suspended.registers.len();
        if let Err(kind) = self.machine.borrow_mut().enter_native_generator() {
            self.machine
                .borrow_mut()
                .release_suspended_activation_registers(register_count);
            self.pending_fatal_kind.set(Some(kind));
            return None;
        }
        self.activations.borrow_mut().push(Activation {
            this_value: suspended.this_value,
            new_target: suspended.new_target,
            args: suspended.args.clone(),
            arguments_object: suspended.arguments_object,
            pending_resume: None,
        });
        self.push_native_roots(&suspended.registers);
        let completion = self.run_frame(
            target.module,
            index,
            code,
            &mut suspended.registers,
            FrameDrive::GeneratorResume {
                token: suspended.resume_token,
                sent,
            },
        );
        self.pop_native_roots();
        let activation = self
            .activations
            .borrow_mut()
            .pop()
            .expect("generator activation exists");
        self.machine.borrow_mut().leave_native_generator();
        self.finish_reference_generator(target, suspended.registers, activation, completion)
    }

    fn finish_reference_generator(
        &self,
        target: crate::RuntimeFunction,
        registers: Vec<Value>,
        activation: Activation,
        completion: Result<FrameCompletion, RuntimeError>,
    ) -> Option<GeneratorResume> {
        let register_count = registers.len();
        match completion {
            Ok(FrameCompletion::Suspend(value, resume_token)) => Some(GeneratorResume::Yield {
                value,
                activation: SuspendedActivation {
                    target,
                    registers,
                    this_value: activation.this_value,
                    new_target: activation.new_target,
                    args: activation.args,
                    arguments_object: activation.arguments_object,
                    resume_token,
                },
            }),
            Ok(FrameCompletion::Normal(value)) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                Some(GeneratorResume::Return(value))
            }
            Ok(FrameCompletion::Unwind(value, origin, _)) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                Some(GeneratorResume::Throw { value, origin })
            }
            Err(error) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                self.pending_error.set(Some(error));
                None
            }
        }
    }

    fn start_linked_generator(&self, start: GeneratorStart) -> Option<GeneratorResume> {
        let index = start.target.function.get() as usize;
        let handle = self.code_ref(start.target.module);
        let code = handle.code(start.target.module);
        let registers = self.seed_registers(code, index, &start.captures, &start.args);
        if let Err(kind) = self
            .machine
            .borrow_mut()
            .reserve_suspended_activation_registers(registers.len())
        {
            self.pending_fatal_kind.set(Some(kind));
            return None;
        }
        self.drive_linked_generator(
            SuspendedActivation {
                target: start.target,
                registers,
                this_value: start.this_value,
                new_target: start.new_target,
                args: start.args,
                arguments_object: None,
                resume_token: 0,
            },
            None,
        )
    }

    fn resume_linked_generator(
        &self,
        activation: SuspendedActivation,
        sent: Value,
    ) -> Option<GeneratorResume> {
        self.drive_linked_generator(activation, Some(sent))
    }

    fn drive_linked_generator(
        &self,
        mut suspended: SuspendedActivation,
        pending_resume: Option<Value>,
    ) -> Option<GeneratorResume> {
        let register_count = suspended.registers.len();
        if let Err(kind) = self.machine.borrow_mut().enter_native_generator() {
            self.machine
                .borrow_mut()
                .release_suspended_activation_registers(register_count);
            self.pending_fatal_kind.set(Some(kind));
            return None;
        }
        let length = match u16::try_from(register_count) {
            Ok(length) => length,
            Err(_) => {
                self.machine.borrow_mut().leave_native_generator();
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                self.pending_fatal_kind
                    .set(Some(RuntimeErrorKind::RegisterLimitExceeded {
                        limit: self.max_total_registers(),
                    }));
                return None;
            }
        };
        self.activations.borrow_mut().push(Activation {
            this_value: suspended.this_value,
            new_target: suspended.new_target,
            args: suspended.args.clone(),
            arguments_object: suspended.arguments_object,
            pending_resume,
        });
        self.push_native_roots(&suspended.registers);
        let handles = suspended.registers.as_mut_ptr();
        let (invoked, next_token, out) = {
            let mut shadow = ShadowFrame::new(
                std::ptr::null_mut(),
                suspended.resume_token,
                suspended.target.module.get(),
                handles,
                length,
            );
            let mut out = Completion::new(Value::UNDEFINED);
            let invoked = self.entries.invoke(
                suspended.target.module.get(),
                suspended.target.function.get(),
                &mut shadow,
                &mut out,
            );
            (invoked, shadow.bytecode_pc, out)
        };
        self.pop_native_roots();
        let activation = self
            .activations
            .borrow_mut()
            .pop()
            .expect("generator activation exists");
        self.machine.borrow_mut().leave_native_generator();
        suspended.arguments_object = activation.arguments_object;

        match invoked {
            Ok(CompletionTag::Suspend) if next_token != 0 => {
                suspended.resume_token = next_token;
                Some(GeneratorResume::Yield {
                    value: out.value,
                    activation: suspended,
                })
            }
            Ok(CompletionTag::Normal) => {
                self.pending_throw.take();
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                Some(GeneratorResume::Return(out.value))
            }
            Ok(CompletionTag::Throw) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                let (value, origin) = self.take_matching_throw(out.value);
                Some(GeneratorResume::Throw { value, origin })
            }
            Ok(CompletionTag::Suspend | CompletionTag::FatalTrap) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                None
            }
            Err(error) => {
                self.machine
                    .borrow_mut()
                    .release_suspended_activation_registers(register_count);
                self.pending_abi_error.set(Some(error));
                None
            }
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
        debug_assert!(!self.is_dynamic_module(target.module));
        let index = target.function.get() as usize;
        let handle = self.code_ref(target.module);
        let code = handle.code(target.module);
        let register_count = code.functions()[index].register_count() as usize;
        let mut registers = self.seed_registers(code, index, captures, args);
        let length = match u16::try_from(registers.len()) {
            Ok(length) => length,
            Err(_) => {
                let limit = self.max_total_registers();
                self.pending_fatal_kind
                    .set(Some(RuntimeErrorKind::RegisterLimitExceeded { limit }));
                return InvokeOutcome::Fatal;
            }
        };
        if let Err(kind) = self
            .machine
            .borrow_mut()
            .reserve_native_activation(register_count)
        {
            self.pending_fatal_kind.set(Some(kind));
            return InvokeOutcome::Fatal;
        }
        self.activations.borrow_mut().push(Activation {
            this_value: this,
            new_target,
            args: args.to_vec(),
            arguments_object: None,
            pending_resume: None,
        });
        self.push_native_roots(&registers);
        let handles = registers.as_mut_ptr();
        let (tag, out) = {
            let mut shadow = ShadowFrame::new(
                std::ptr::null_mut(),
                0,
                target.module.get(),
                handles,
                length,
            );
            let mut out = Completion::new(Value::UNDEFINED);
            let tag = self.entries.invoke(
                target.module.get(),
                target.function.get(),
                &mut shadow,
                &mut out,
            );
            (tag, out)
        };
        drop(registers);
        self.pop_native_roots();
        self.activations.borrow_mut().pop();
        self.machine
            .borrow_mut()
            .release_native_activation(register_count);
        match tag {
            Ok(CompletionTag::Normal) => {
                self.pending_throw.take();
                InvokeOutcome::Value(out.value)
            }
            Ok(CompletionTag::Throw) => {
                let (value, origin) = self.take_matching_throw(out.value);
                InvokeOutcome::Threw(value, origin)
            }
            Ok(CompletionTag::Suspend | CompletionTag::FatalTrap) => InvokeOutcome::Fatal,
            Err(error) => {
                self.pending_abi_error.set(Some(error));
                InvokeOutcome::Fatal
            }
        }
    }

    fn evaluate_import(&self, module: ModuleId) -> Result<(), ImportFailure> {
        let begun = self.machine.borrow_mut().begin_module_evaluation(module);
        let dependencies = match begun {
            Err(error) => {
                self.pending_error.set(Some(error));
                return Err(ImportFailure::Fatal);
            }
            Ok(crate::ModuleEvaluation::Cycle) => return Ok(()),
            Ok(crate::ModuleEvaluation::Evaluated(Ok(()))) => return Ok(()),
            Ok(crate::ModuleEvaluation::Evaluated(Err(error))) => {
                if matches!(error.kind, RuntimeErrorKind::UncaughtThrow { .. }) {
                    return Err(ImportFailure::Threw(error));
                }
                self.pending_error.set(Some(error));
                return Err(ImportFailure::Fatal);
            }
            Ok(crate::ModuleEvaluation::Ready(dependencies)) => dependencies,
        };
        for dependency in dependencies {
            if let Err(failure) = self.evaluate_import(dependency) {
                let mut machine = self.machine.borrow_mut();
                match &failure {
                    ImportFailure::Threw(error) => {
                        machine.settle_module_evaluation(module, Err(error.clone()));
                    }
                    ImportFailure::Fatal => machine.abort_module_evaluation(module),
                }
                return Err(failure);
            }
        }

        let function = self.module(module).entry();
        let outcome = self.invoke_runtime(
            crate::RuntimeFunction { module, function },
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
        );
        match outcome {
            InvokeOutcome::Value(_) => {
                self.machine
                    .borrow_mut()
                    .settle_module_evaluation(module, Ok(()));
                Ok(())
            }
            InvokeOutcome::Threw(value, origin) => {
                let error = self.error_at(
                    module,
                    RuntimeErrorKind::UncaughtThrow { value, origin },
                    function.get() as usize,
                    0,
                );
                self.machine
                    .borrow_mut()
                    .settle_module_evaluation(module, Err(error.clone()));
                Err(ImportFailure::Threw(error))
            }
            InvokeOutcome::Fatal => {
                self.machine.borrow_mut().abort_module_evaluation(module);
                Err(ImportFailure::Fatal)
            }
        }
    }

    fn import_namespace(&self, requester: ModuleId, specifier: u32) -> HelperResult {
        let target = self
            .machine
            .borrow()
            .resolve_import(requester, ConstantId::new(specifier));
        let target = match target {
            Ok(target) => target,
            Err(kind) => return self.fatal(kind),
        };
        if let crate::ImportTarget::Local(module) = target
            && let Err(failure) = self.evaluate_import(module)
        {
            return match failure {
                ImportFailure::Threw(error) => self.fail(crate::import_failure(&error)),
                ImportFailure::Fatal => HelperResult {
                    tag: CompletionTag::FatalTrap,
                    value: Value::UNDEFINED,
                },
            };
        }
        let namespace = self
            .machine
            .borrow_mut()
            .imported_namespace(requester, target);
        match namespace {
            Ok(value) => HelperResult::normal(value),
            Err(kind) => self.fatal(kind),
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
            EvalFailure::ThrowValueOrigin { value, origin } => {
                self.pending_throw.set(Some(PendingThrow { value, origin }));
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

    fn constant_text(&self, module: ModuleId, id: u32) -> EcmaString {
        self.machine
            .borrow()
            .constant_text(module, ConstantId::new(id))
            .clone()
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
        let mut callee = callee;
        let mut arguments = Cow::Borrowed(arguments);
        if matches!(
            self.machine.borrow().callee_kind(callee),
            Ok(CalleeKind::Bound)
        ) {
            let bound =
                self.machine
                    .borrow()
                    .flatten_bound(callee, Value::UNDEFINED, arguments.as_ref());
            match bound {
                Ok(bound) => {
                    callee = bound.target;
                    arguments = Cow::Owned(bound.arguments);
                }
                Err(kind) => return self.fatal(kind),
            }
        }
        let kind = self.machine.borrow().callee_kind(callee);
        match kind {
            Ok(CalleeKind::Builtin { id }) => {
                let result = self.machine.borrow_mut().call_builtin(
                    id,
                    Value::UNDEFINED,
                    arguments.as_ref(),
                    true,
                );
                match result {
                    Ok(BuiltinOutcome::Value(value)) => HelperResult::normal(value),
                    Ok(
                        BuiltinOutcome::Call { .. }
                        | BuiltinOutcome::GeneratorNext { .. }
                        | BuiltinOutcome::AsyncGeneratorNext { .. },
                    ) => {
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
                let flags = {
                    let handle = self.code_ref(target.module);
                    handle.code(target.module).functions()[target.function.get() as usize].flags()
                };
                if flags.is_async && !flags.is_generator {
                    self.pending_throw.set(Some(PendingThrow {
                        value: Value::UNDEFINED,
                        origin: ThrowOrigin::TypeError {
                            operation: "construct",
                        },
                    }));
                    return HelperResult::throw(Value::UNDEFINED);
                }
                let instance = {
                    let allocated = self
                        .machine
                        .borrow_mut()
                        .allocate_constructed_receiver(callee);
                    match allocated {
                        Ok(value) => value,
                        Err(kind) => return self.fatal(kind),
                    }
                };
                let outcome =
                    self.invoke_runtime(target, &captures, instance, callee, arguments.as_ref());
                match outcome {
                    InvokeOutcome::Value(returned) => {
                        let is_object = self.machine.borrow().is_object(returned);
                        HelperResult::normal(if is_object { returned } else { instance })
                    }
                    other => self.outcome_result(other),
                }
            }
            Ok(CalleeKind::Bound) => self.fatal(RuntimeErrorKind::InvalidValue { value: callee }),
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
        let execution = if self.machine.borrow().module_graph_suspends(module) {
            self.machine
                .borrow_mut()
                .evaluate_instantiated_module(module)
                .map_err(NativeError::Runtime)?
        } else {
            self.evaluate_linked_module(module)?.ok_or_else(|| {
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
            })?
        };
        // The linked backend shares the interpreter's automatic loop policy: it
        // drives the machine to quiescence after successful evaluation. Driver
        // failures are runtime failures, surfaced through `NativeError::Runtime`.
        self.machine
            .borrow_mut()
            .run_to_quiescence()
            .map_err(NativeError::Runtime)?;
        Ok(execution.outcome)
    }

    fn evaluate_linked_module(
        &mut self,
        module: ModuleId,
    ) -> Result<Option<Execution>, NativeError> {
        let dependencies = match self.machine.borrow_mut().begin_module_evaluation(module)? {
            crate::ModuleEvaluation::Cycle => return Ok(None),
            crate::ModuleEvaluation::Evaluated(result) => {
                return result.map(|()| None).map_err(Into::into);
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

        let result = if self.machine.borrow().module_entry_suspends(module) {
            self.machine
                .borrow_mut()
                .run_module_entry(module)
                .map_err(NativeError::Runtime)
        } else {
            self.invoke_linked_entry(module)
        };

        match result {
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
        let code = self.module(module);
        let function_id = code.entry();
        let function = function_id.get() as usize;
        let register_count = code.functions()[function].register_count() as usize;
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
        let mut registers = self.seed_registers(code, function, &[], &[]);
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
        let reserved = self
            .machine
            .borrow_mut()
            .reserve_native_activation(register_count);
        reserved.map_err(|kind| NativeError::Runtime(self.error_at(module, kind, function, 0)))?;
        self.activations.borrow_mut().push(Activation {
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            pending_resume: None,
        });
        self.push_native_roots(&registers);
        let handles = registers.as_mut_ptr();
        let entries = self.entries;
        let (tag, out, fault_pc) = {
            let mut shadow =
                ShadowFrame::new(std::ptr::null_mut(), 0, module.get(), handles, length);
            let mut out = Completion::new(Value::UNDEFINED);
            let tag = with_native_ops(self, || {
                entries.invoke(module.get(), function_id.get(), &mut shadow, &mut out)
            });
            (tag, out, shadow.bytecode_pc as usize)
        };
        self.pop_native_roots();
        self.activations.borrow_mut().pop();
        self.machine
            .borrow_mut()
            .release_native_activation(register_count);
        match tag {
            Ok(CompletionTag::Normal) => {
                self.pending_throw.take();
                Ok(Execution {
                    outcome: ExecutionOutcome {
                        stdout: self.stdout.borrow().clone(),
                        exit_code: self.exit_code.get(),
                    },
                    value: out.value,
                    link: out.value,
                    entry_registers: registers,
                })
            }
            Ok(CompletionTag::Throw) => {
                let (value, origin) = self.take_matching_throw(out.value);
                Err(NativeError::Runtime(self.error_at(
                    module,
                    RuntimeErrorKind::UncaughtThrow { value, origin },
                    function,
                    fault_pc,
                )))
            }
            Ok(CompletionTag::Suspend | CompletionTag::FatalTrap) => {
                if let Some(error) = self.pending_abi_error.take() {
                    Err(NativeError::Abi(error))
                } else if let Some(error) = self.pending_error.take() {
                    Err(NativeError::Runtime(error))
                } else if let Some(kind) = self.pending_fatal_kind.take() {
                    Err(NativeError::Runtime(
                        self.error_at(module, kind, function, fault_pc),
                    ))
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
        self.prepare_native_helper(frame);
        let module = ModuleId::new(frame.module_id());
        let amount = match call {
            HelperCall::ResumeValue => None,
            HelperCall::ConsumeFuel { amount } => Some(u64::from(amount)),
            _ => Some(1),
        };
        if let Some(amount) = amount
            && let Err(kind) = self.machine.borrow_mut().consume_fuel(amount)
        {
            return self.fatal(kind);
        }
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
            HelperCall::CreateCell => {
                let prototype = self.machine.borrow().intrinsics.array_prototype;
                self.allocated(HeapEntry::Array {
                    elements: vec![Value::UNINITIALIZED],
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
            HelperCall::Import { specifier } => self.import_namespace(module, specifier),
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
                    .borrow_mut()
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
                    Err(failure) => self.fail(failure),
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
                    .borrow_mut()
                    .load_global(module, ConstantId::new(name));
                let text = match resolved {
                    Ok(Some(value)) => EcmaString::from_utf8(self.machine.borrow().type_of(value)),
                    Ok(None) => EcmaString::from_utf8("undefined"),
                    Err(failure) => return self.fail(failure),
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
                let outcome = self.array_extend_active(array, iterable);
                self.outcome_result(outcome)
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
                let prototype = self.machine.borrow().intrinsics.regexp_prototype();
                self.allocated(HeapEntry::RegExp {
                    pattern,
                    flags,
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                    extensible: true,
                })
            }
            HelperCall::GetIterator { src, kind } => match iterator_kind_from_selector(kind) {
                Some(kind) => self.outcome_result(self.get_iterator_active(src, kind)),
                None => self.fatal(RuntimeErrorKind::InvalidValue {
                    value: Value::UNDEFINED,
                }),
            },
            HelperCall::IteratorNext {
                iterator,
                done_reg,
                value_reg,
            } => match self.iterator_next_active(iterator) {
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
                Err(outcome) => self.outcome_result(outcome),
            },
            HelperCall::IteratorStep { iterator } => match self.iterator_step_active(iterator) {
                Ok(result) => HelperResult::normal(result),
                Err(outcome) => self.outcome_result(outcome),
            },
            HelperCall::IteratorResult {
                result,
                done_reg,
                value_reg,
            } => match self.iterator_result_active(result) {
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
                Err(outcome) => self.outcome_result(outcome),
            },
            HelperCall::IteratorClose {
                iterator,
                mode,
                called_reg,
            } => match self.close_iterator_active(frame, iterator, mode, called_reg) {
                Ok(result) => HelperResult::normal(result),
                Err(outcome) => self.outcome_result(outcome),
            },
            HelperCall::RequireCloseResult { result, called } => {
                if called == Value::TRUE && !self.machine.borrow().is_object(result) {
                    self.fail(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "iterator.return() returned a non-object",
                    }))
                } else {
                    HelperResult::normal(Value::UNDEFINED)
                }
            }
            HelperCall::Export { .. } => self.fail(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "export outside an engine-owned module registry",
            })),
            HelperCall::ConsumeFuel { .. } => HelperResult::normal(Value::UNDEFINED),
        }
    }
}

fn is_inline_instruction(instruction: Instruction) -> bool {
    match instruction {
        Instruction::Move { .. }
        | Instruction::Jump { .. }
        | Instruction::JumpIfTrue { .. }
        | Instruction::JumpIfFalse { .. }
        | Instruction::Return { .. }
        | Instruction::Halt
        | Instruction::Throw { .. }
        | Instruction::Suspend { .. }
        | Instruction::Await { .. } => true,
        Instruction::LoadConst { .. }
        | Instruction::Unary { .. }
        | Instruction::Binary { .. }
        | Instruction::CreateObject { .. }
        | Instruction::CreateArray { .. }
        | Instruction::CreateCell { .. }
        | Instruction::CreateClosure { .. }
        | Instruction::GetProperty { .. }
        | Instruction::SetProperty { .. }
        | Instruction::DeleteProperty { .. }
        | Instruction::DefineAccessor { .. }
        | Instruction::Call { .. }
        | Instruction::Construct { .. }
        | Instruction::LoadGlobal { .. }
        | Instruction::StoreGlobal { .. }
        | Instruction::TypeOfGlobal { .. }
        | Instruction::LoadThis { .. }
        | Instruction::LoadArguments { .. }
        | Instruction::LoadNewTarget { .. }
        | Instruction::ArrayPush { .. }
        | Instruction::ArrayExtend { .. }
        | Instruction::ObjectSpread { .. }
        | Instruction::SetPrototype { .. }
        | Instruction::CreatePrivateName { .. }
        | Instruction::CreateRegExp { .. }
        | Instruction::GetIterator { .. }
        | Instruction::IteratorNext { .. }
        | Instruction::IteratorStep { .. }
        | Instruction::IteratorResult { .. }
        | Instruction::IteratorClose { .. }
        | Instruction::RequireCloseResult { .. }
        | Instruction::Import { .. }
        | Instruction::Export { .. } => false,
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
    use std::sync::Arc;

    use bamts_bytecode::{
        BinaryOp, Binding, BindingId, BindingKind, Constant, ConstantId, Edge, EdgeId, EdgeKind,
        EdgeTarget, ExceptionHandler, Export, ExportSource, Function, FunctionFlags, FunctionId,
        Instruction, IteratorKind, Module, ModuleId, Pc, Program, ProgramModule, Register,
        Verified,
    };
    use bamts_native::{
        AbiError, Completion, CompletionTag, HelperCall, HelperResult, NativeEntryTable,
        NativeFrame, NativeOps, ShadowFrame, Value,
    };

    use crate::{
        GeneratorState, HeapEntry, Host, Limits, Machine, PropertyMap, RuntimeError,
        RuntimeErrorKind, ThrowOrigin,
    };

    use super::EcmaString;
    use super::{
        Activation, Backend, InvokeOutcome, NativeEngine, NativeError, PendingThrow,
        run_linked_program,
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

    fn receiver_sum_function() -> Function {
        module_function(
            0,
            5,
            vec![
                Instruction::LoadThis { dst: reg(0) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(1),
                },
                Instruction::GetProperty {
                    dst: reg(2),
                    object: reg(0),
                    key: reg(1),
                },
                Instruction::LoadArguments { dst: reg(3) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::GetProperty {
                    dst: reg(4),
                    object: reg(3),
                    key: reg(1),
                },
                Instruction::Binary {
                    dst: reg(2),
                    op: BinaryOp::Add,
                    left: reg(2),
                    right: reg(4),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(3),
                },
                Instruction::GetProperty {
                    dst: reg(4),
                    object: reg(3),
                    key: reg(1),
                },
                Instruction::Binary {
                    dst: reg(2),
                    op: BinaryOp::Add,
                    left: reg(2),
                    right: reg(4),
                },
                Instruction::Return { value: reg(2) },
            ],
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
        constants.insert(0, Constant::String(EcmaString::from_utf8(name)));
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

    fn dynamic_cycle_program() -> Program<Verified> {
        let root = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("./target"))],
            vec![entry_function(
                3,
                vec![
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(1),
                    },
                    Instruction::Import {
                        dst: reg(1),
                        specifier: cid(1),
                    },
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::StrictEqual,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Return { value: reg(2) },
                ],
            )],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            Vec::new(),
        );
        let target = program_module(
            "target",
            vec![Constant::String(EcmaString::from_utf8("./root"))],
            vec![entry_function(1, vec![Instruction::Halt])],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            Vec::new(),
            Vec::new(),
        );
        linked(vec![root, target], 0)
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

    #[test]
    fn microtask_checkpoint_matches_interpreter_and_native_engine() {
        let program = linked(
            vec![program_module(
                "root",
                vec![
                    Constant::String(EcmaString::from_utf8("queueMicrotask")),
                    Constant::String(EcmaString::from_utf8("observed")),
                    Constant::Int32(7),
                    Constant::Undefined,
                ],
                vec![
                    entry_function(
                        6,
                        vec![
                            Instruction::CreateArray { dst: reg(0) },
                            Instruction::CreateClosure {
                                dst: reg(1),
                                function: FunctionId::new(1),
                                captures: reg(0),
                            },
                            Instruction::LoadGlobal {
                                dst: reg(2),
                                name: cid(1),
                            },
                            Instruction::CreateArray { dst: reg(3) },
                            Instruction::ArrayPush {
                                array: reg(3),
                                value: reg(1),
                            },
                            Instruction::LoadConst {
                                dst: reg(4),
                                constant: cid(4),
                            },
                            Instruction::Call {
                                dst: reg(5),
                                callee: reg(2),
                                this_value: reg(4),
                                arguments: reg(3),
                            },
                            Instruction::Return { value: reg(4) },
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
                                name: cid(2),
                                value: reg(0),
                            },
                            Instruction::Return { value: reg(0) },
                        ],
                    ),
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )],
            0,
        );
        let observed = EcmaString::from_utf8("observed");

        let mut interpreter_host = SilentHost;
        let mut interpreter = Machine::new(&program, &mut interpreter_host, Limits::default());
        interpreter.evaluate().unwrap();
        assert!(!interpreter.globals.contains_key(&observed));
        let interpreter_drain = interpreter.drain_microtasks().unwrap();
        let interpreter_value = interpreter.globals.get(&observed).copied();

        let mut native_host = SilentHost;
        let engine = NativeEngine::new(&program, &NoEntries, &mut native_host, Limits::default());
        engine.machine.borrow_mut().instantiate_modules().unwrap();
        engine
            .evaluate_reference_module(program.entry())
            .unwrap()
            .expect("native entry completes");
        assert!(!engine.machine.borrow().globals.contains_key(&observed));
        let native_drain = engine.machine.borrow_mut().drain_microtasks().unwrap();
        let native_value = engine.machine.borrow().globals.get(&observed).copied();

        assert_eq!(interpreter_drain, native_drain);
        assert_eq!(interpreter_value, Some(Value::int32(7)));
        assert_eq!(native_value, interpreter_value);
    }

    #[derive(Default)]
    struct RecordingHost {
        stdout: Vec<u8>,
    }

    impl Host for RecordingHost {
        fn write_stdout(&mut self, bytes: &[u8]) {
            self.stdout.extend_from_slice(bytes);
        }
    }

    fn first_window(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn automatic_loop_program(callback: Function) -> Program<Verified> {
        linked(
            vec![program_module(
                "root",
                vec![
                    Constant::String(EcmaString::from_utf8("console")),
                    Constant::String(EcmaString::from_utf8("log")),
                    Constant::String(EcmaString::from_utf8("sync")),
                    Constant::String(EcmaString::from_utf8("async")),
                    Constant::String(EcmaString::from_utf8("queueMicrotask")),
                    Constant::Int32(42),
                    Constant::Undefined,
                ],
                vec![
                    entry_function(
                        7,
                        vec![
                            Instruction::LoadGlobal {
                                dst: reg(0),
                                name: cid(1),
                            },
                            Instruction::LoadConst {
                                dst: reg(2),
                                constant: cid(3),
                            },
                            Instruction::LoadConst {
                                dst: reg(1),
                                constant: cid(2),
                            },
                            Instruction::GetProperty {
                                dst: reg(1),
                                object: reg(0),
                                key: reg(1),
                            },
                            Instruction::CreateArray { dst: reg(3) },
                            Instruction::ArrayPush {
                                array: reg(3),
                                value: reg(2),
                            },
                            Instruction::LoadConst {
                                dst: reg(4),
                                constant: cid(7),
                            },
                            Instruction::Call {
                                dst: reg(5),
                                callee: reg(1),
                                this_value: reg(4),
                                arguments: reg(3),
                            },
                            Instruction::CreateArray { dst: reg(3) },
                            Instruction::CreateClosure {
                                dst: reg(1),
                                function: FunctionId::new(1),
                                captures: reg(3),
                            },
                            Instruction::LoadGlobal {
                                dst: reg(6),
                                name: cid(5),
                            },
                            Instruction::CreateArray { dst: reg(3) },
                            Instruction::ArrayPush {
                                array: reg(3),
                                value: reg(1),
                            },
                            Instruction::LoadConst {
                                dst: reg(4),
                                constant: cid(7),
                            },
                            Instruction::Call {
                                dst: reg(5),
                                callee: reg(6),
                                this_value: reg(4),
                                arguments: reg(3),
                            },
                            Instruction::LoadConst {
                                dst: reg(0),
                                constant: cid(6),
                            },
                            Instruction::Return { value: reg(0) },
                        ],
                    ),
                    callback,
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )],
            0,
        )
    }

    fn queue_callback_microtask<H: Host>(
        engine: &mut NativeEngine<'_, '_, H>,
        function: FunctionId,
    ) {
        let mut machine = engine.machine.borrow_mut();
        let prototype = machine.intrinsics.function_prototype;
        let callback = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function,
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                extensible: true,
            })
            .expect("microtask callback allocates");
        let queue = machine
            .intrinsics
            .global("queueMicrotask")
            .expect("queueMicrotask is installed");
        machine
            .call_value(queue, Value::UNDEFINED, &[callback])
            .expect("microtask enqueues");
    }

    #[test]
    fn automatic_loop_drains_microtasks_and_preserves_synchronous_result() {
        let program = automatic_loop_program(module_function(
            0,
            4,
            vec![
                Instruction::LoadGlobal {
                    dst: reg(0),
                    name: cid(1),
                },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(4),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::GetProperty {
                    dst: reg(1),
                    object: reg(0),
                    key: reg(1),
                },
                Instruction::CreateArray { dst: reg(3) },
                Instruction::ArrayPush {
                    array: reg(3),
                    value: reg(2),
                },
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(7),
                },
                Instruction::Call {
                    dst: reg(2),
                    callee: reg(1),
                    this_value: reg(0),
                    arguments: reg(3),
                },
                Instruction::Return { value: reg(0) },
            ],
        ));

        let mut interpreter_host = RecordingHost::default();
        let interpreter = Machine::new(&program, &mut interpreter_host, Limits::default())
            .run()
            .expect("interpreter drains to quiescence");
        assert_eq!(interpreter.value, Value::int32(42));
        assert!(interpreter.outcome.stdout.is_empty());
        let interp_sync = first_window(&interpreter_host.stdout, b"sync");
        let interp_async = first_window(&interpreter_host.stdout, b"async");
        assert!(
            interp_sync.is_some_and(|s| interp_async.is_some_and(|a| s < a)),
            "host received sync-then-async bytes: {:?}",
            interpreter_host.stdout,
        );

        let mut native_host = RecordingHost::default();
        let native = NativeEngine::new(&program, &NoEntries, &mut native_host, Limits::default())
            .run()
            .expect("reference native drains to quiescence");
        assert_eq!(native.value, Value::int32(42));
        assert!(native.outcome.stdout.is_empty());
        assert_eq!(native_host.stdout, interpreter_host.stdout);

        let mut linked_host = RecordingHost::default();
        let linked_outcome = {
            let entries = ForeignEntries {
                program_bytes: program.encode(),
                invoked: Cell::new(false),
            };
            let mut linked = NativeEngine::build(
                &program,
                &entries,
                &mut linked_host,
                Limits::default(),
                Backend::Linked,
            );
            queue_callback_microtask(&mut linked, FunctionId::new(1));
            linked
                .run_linked()
                .expect("linked native drains to quiescence")
        };
        assert!(linked_outcome.stdout.is_empty());
        assert!(
            first_window(&linked_host.stdout, b"async").is_some(),
            "linked loop drained the pre-queued microtask: {:?}",
            linked_host.stdout,
        );
    }

    #[test]
    fn automatic_loop_surfaces_uncaught_callback_error_across_entrypoints() {
        let program = automatic_loop_program(module_function(
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(6),
                },
                Instruction::Throw { value: reg(0) },
            ],
        ));

        let mut interpreter_host = SilentHost;
        let interpreter_err = Machine::new(&program, &mut interpreter_host, Limits::default())
            .run()
            .expect_err("interpreter surfaces the uncaught callback");
        let thrown = match &interpreter_err.kind {
            RuntimeErrorKind::UncaughtThrow { value, .. } => *value,
            other => panic!("expected UncaughtThrow, got {other:?}"),
        };
        assert_eq!(thrown, Value::int32(42));

        let mut native_host = SilentHost;
        let native_err =
            NativeEngine::new(&program, &NoEntries, &mut native_host, Limits::default())
                .run()
                .expect_err("reference native surfaces the uncaught callback");
        assert_eq!(native_err.kind, interpreter_err.kind);

        let mut linked_host = SilentHost;
        let linked_err = {
            let entries = ForeignEntries {
                program_bytes: program.encode(),
                invoked: Cell::new(false),
            };
            let mut linked = NativeEngine::build(
                &program,
                &entries,
                &mut linked_host,
                Limits::default(),
                Backend::Linked,
            );
            queue_callback_microtask(&mut linked, FunctionId::new(1));
            linked
                .run_linked()
                .expect_err("linked native surfaces the uncaught callback")
        };
        match linked_err {
            NativeError::Runtime(RuntimeError { ref kind, .. })
                if *kind == interpreter_err.kind => {}
            other => panic!("linked mapped the callback throw: {other:?}"),
        }
    }

    fn async_module_function(registers: u32, code: Vec<Instruction>) -> Function {
        Function::new(
            None,
            0,
            0,
            registers,
            FunctionFlags {
                is_async: true,
                is_generator: false,
            },
            code,
            Vec::new(),
        )
    }

    /// An ordinary async call routed through the native reference engine must
    /// produce the same Promise settlement, globals, and drain counts as the
    /// interpreter, because native async execution reuses the shared Machine
    /// reference state machine.
    #[test]
    fn async_call_matches_interpreter_and_native_engine() {
        let program = linked(
            vec![program_module(
                "root",
                vec![
                    Constant::String(EcmaString::from_utf8("observed")),
                    Constant::Int32(7),
                    Constant::Undefined,
                ],
                vec![
                    entry_function(
                        5,
                        vec![
                            Instruction::CreateArray { dst: reg(0) },
                            Instruction::CreateClosure {
                                dst: reg(1),
                                function: FunctionId::new(1),
                                captures: reg(0),
                            },
                            Instruction::CreateArray { dst: reg(2) },
                            Instruction::LoadConst {
                                dst: reg(3),
                                constant: cid(3),
                            },
                            Instruction::Call {
                                dst: reg(4),
                                callee: reg(1),
                                this_value: reg(3),
                                arguments: reg(2),
                            },
                            Instruction::Return { value: reg(3) },
                        ],
                    ),
                    async_module_function(
                        2,
                        vec![
                            Instruction::LoadConst {
                                dst: reg(0),
                                constant: cid(2),
                            },
                            Instruction::Await {
                                dst: reg(1),
                                src: reg(0),
                                resume: pc(2),
                            },
                            Instruction::StoreGlobal {
                                name: cid(1),
                                value: reg(1),
                            },
                            Instruction::Return { value: reg(1) },
                        ],
                    ),
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )],
            0,
        );
        let observed = EcmaString::from_utf8("observed");

        let mut interpreter_host = SilentHost;
        let mut interpreter = Machine::new(&program, &mut interpreter_host, Limits::default());
        interpreter.evaluate().unwrap();
        assert!(!interpreter.globals.contains_key(&observed));
        let interpreter_drain = interpreter.drain_microtasks().unwrap();
        let interpreter_value = interpreter.globals.get(&observed).copied();

        let mut native_host = SilentHost;
        let engine = NativeEngine::new(&program, &NoEntries, &mut native_host, Limits::default());
        engine.machine.borrow_mut().instantiate_modules().unwrap();
        engine
            .evaluate_reference_module(program.entry())
            .unwrap()
            .expect("native entry completes");
        assert!(!engine.machine.borrow().globals.contains_key(&observed));
        let native_drain = engine.machine.borrow_mut().drain_microtasks().unwrap();
        let native_value = engine.machine.borrow().globals.get(&observed).copied();

        let mut linked_host = SilentHost;
        let linked = NativeEngine::build(
            &program,
            &NoEntries,
            &mut linked_host,
            Limits::default(),
            Backend::Linked,
        );
        linked.machine.borrow_mut().instantiate_modules().unwrap();
        assert!(matches!(
            linked.invoke_runtime(
                crate::RuntimeFunction {
                    module: ModuleId::new(0),
                    function: FunctionId::new(1),
                },
                &[],
                Value::UNDEFINED,
                Value::UNDEFINED,
                &[],
            ),
            InvokeOutcome::Value(_)
        ));
        assert!(!linked.machine.borrow().globals.contains_key(&observed));
        let linked_drain = linked.machine.borrow_mut().drain_microtasks().unwrap();
        let linked_value = linked.machine.borrow().globals.get(&observed).copied();

        assert_eq!(interpreter_drain, native_drain);
        assert_eq!(interpreter_value, Some(Value::int32(7)));
        assert_eq!(native_value, interpreter_value);
        assert_eq!(linked_drain, interpreter_drain);
        assert_eq!(linked_value, interpreter_value);
    }

    /// Constructing an async function through the native engine is a TypeError,
    /// matching the interpreter's construct guard, with no bytecode/ABI change.
    #[test]
    fn native_construct_of_async_function_is_a_type_error() {
        let program = linked(
            vec![program_module(
                "root",
                Vec::new(),
                vec![
                    entry_function(
                        4,
                        vec![
                            Instruction::CreateArray { dst: reg(0) },
                            Instruction::CreateClosure {
                                dst: reg(1),
                                function: FunctionId::new(1),
                                captures: reg(0),
                            },
                            Instruction::CreateArray { dst: reg(2) },
                            Instruction::Construct {
                                dst: reg(3),
                                callee: reg(1),
                                arguments: reg(2),
                            },
                            Instruction::Return { value: reg(3) },
                        ],
                    ),
                    async_module_function(1, vec![Instruction::Halt]),
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )],
            0,
        );
        let mut host = SilentHost;
        let result = NativeEngine::new(&program, &NoEntries, &mut host, Limits::default()).run();
        assert!(matches!(
            result,
            Err(RuntimeError {
                kind: RuntimeErrorKind::UncaughtThrow {
                    origin: ThrowOrigin::TypeError { .. },
                    ..
                },
                ..
            })
        ));
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
                vec![
                    Constant::String(EcmaString::from_utf8("x")),
                    Constant::Int32(value),
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
                Constant::String(EcmaString::from_utf8("left")),
                Constant::String(EcmaString::from_utf8("right")),
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("one")),
                Constant::String(EcmaString::from_utf8("two")),
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
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::String(EcmaString::from_utf8("set")),
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
                Constant::String(EcmaString::from_utf8("set")),
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("dependency")),
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
                Constant::String(EcmaString::from_utf8("a")),
                Constant::Int32(1),
                Constant::String(EcmaString::from_utf8("second")),
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
                Constant::String(EcmaString::from_utf8("a")),
                Constant::String(EcmaString::from_utf8("first")),
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
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(7),
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
                Constant::String(EcmaString::from_utf8("ns")),
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("dependency")),
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
                Constant::String(EcmaString::from_utf8("count")),
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
                Constant::String(EcmaString::from_utf8("count")),
                Constant::String(EcmaString::from_utf8("dependency")),
                Constant::String(EcmaString::from_utf8("dependency-again")),
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
    fn reference_and_interpreter_charge_each_mixed_instruction_once() {
        let module = verified(
            vec![Constant::Int32(1)],
            vec![entry_function(
                3,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::Move {
                        dst: reg(1),
                        src: reg(0),
                    },
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Jump { target: pc(4) },
                    Instruction::Halt,
                ],
            )],
        );
        let program = one_module_program(&module);

        for fuel in [0, 4, 5] {
            let limits = Limits {
                fuel,
                ..Limits::default()
            };
            let mut interpreter_host = SilentHost;
            let interpreter = Machine::new(&program, &mut interpreter_host, limits.clone()).run();
            let mut reference_host = SilentHost;
            let reference =
                NativeEngine::new(&program, &NoEntries, &mut reference_host, limits).run();
            assert_eq!(interpreter, reference, "fuel={fuel}");
            if fuel == 5 {
                assert!(reference.is_ok(), "N instructions must fit fuel N");
            } else {
                assert!(
                    matches!(
                        reference,
                        Err(RuntimeError {
                            kind: RuntimeErrorKind::FuelExhausted { limit },
                            ..
                        }) if limit == fuel
                    ),
                    "fuel={fuel} must exhaust before the next instruction"
                );
            }
        }
    }

    #[test]
    fn native_reservations_share_interpreter_depth_and_register_ceilings() {
        let module = verified(Vec::new(), vec![entry_function(2, vec![Instruction::Halt])]);
        let program = one_module_program(&module);
        let mut host = SilentHost;
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_call_depth: 2,
                max_total_registers: 3,
                ..Limits::default()
            },
        );

        assert_eq!(machine.frames.len(), 1);
        assert_eq!(machine.live_registers, 2);
        machine.reserve_native_activation(1).unwrap();
        assert_eq!((machine.native_depth, machine.live_registers), (1, 3));
        assert!(matches!(
            machine.reserve_native_activation(0),
            Err(RuntimeErrorKind::CallDepthExceeded { limit: 2 })
        ));
        assert_eq!(
            (machine.native_depth, machine.live_registers),
            (1, 3),
            "failed depth reservation is atomic"
        );
        machine.release_native_activation(1);

        machine.limits.max_call_depth = 3;
        machine.limits.max_total_registers = 2;
        assert!(matches!(
            machine.reserve_native_activation(1),
            Err(RuntimeErrorKind::RegisterLimitExceeded { limit: 2 })
        ));
        assert_eq!(
            (machine.native_depth, machine.live_registers),
            (0, 2),
            "failed register reservation is atomic"
        );
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
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(5),
            ],
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
        constants.push(Constant::String(EcmaString::from_utf8("test-module")));
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
            vec![Constant::String(EcmaString::from_utf8("<test>"))],
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
            vec![Constant::String(EcmaString::from_utf8("dependency"))],
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
    fn linked_top_level_await_uses_the_reference_entry() {
        let module = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("after")),
                Constant::Int32(7),
            ],
            vec![entry_function(
                2,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::Await {
                        dst: reg(1),
                        src: reg(0),
                        resume: pc(2),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(1),
                    },
                    Instruction::Return { value: reg(1) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![module], 0);
        let entries = ForeignEntries {
            program_bytes: program.encode(),
            invoked: Cell::new(false),
        };
        let mut host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );

        engine.run_linked().expect("top-level await completes");

        assert!(!entries.invoked.get(), "linked entry must not run");
        assert_eq!(
            engine
                .machine
                .borrow()
                .globals
                .get(&EcmaString::from_utf8("after")),
            Some(&Value::int32(7)),
        );
    }

    #[test]
    fn static_dependency_tla_delegates_the_whole_native_graph() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("after")),
                Constant::Undefined,
                Constant::Int32(7),
            ],
            vec![entry_function(
                2,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::Await {
                        dst: reg(0),
                        src: reg(0),
                        resume: pc(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(3),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(1),
                    },
                    Instruction::Return { value: reg(1) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("after")),
                Constant::String(EcmaString::from_utf8("dependency")),
            ],
            vec![entry_function(
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
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![dependency, root], 1);

        let mut reference_host = SilentHost;
        let execution =
            NativeEngine::new(&program, &NoEntries, &mut reference_host, Limits::default())
                .run()
                .expect("reference backend completes the async dependency");
        assert_eq!(execution.value, Value::int32(7));

        let entries = ForeignEntries {
            program_bytes: program.encode(),
            invoked: Cell::new(false),
        };
        let mut linked_host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &entries,
            &mut linked_host,
            Limits::default(),
            Backend::Linked,
        );
        engine
            .run_linked()
            .expect("linked backend completes the async dependency");
        assert!(
            !entries.invoked.get(),
            "the linked graph must not mix native and interpreter entries"
        );
        assert_eq!(
            engine
                .machine
                .borrow()
                .globals
                .get(&EcmaString::from_utf8("after")),
            Some(&Value::int32(7)),
        );
    }

    #[test]
    fn first_static_dependency_tla_rejection_skips_the_native_graph() {
        let rejecting = program_module(
            "rejecting",
            vec![Constant::Undefined, Constant::Int32(9)],
            vec![entry_function(
                2,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(1),
                    },
                    Instruction::Await {
                        dst: reg(0),
                        src: reg(0),
                        resume: pc(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(2),
                    },
                    Instruction::Throw { value: reg(1) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let pending = program_module(
            "pending",
            vec![Constant::Undefined],
            vec![entry_function(
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(1),
                    },
                    Instruction::Await {
                        dst: reg(0),
                        src: reg(0),
                        resume: pc(2),
                    },
                    Instruction::Await {
                        dst: reg(0),
                        src: reg(0),
                        resume: pc(3),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("rejecting")),
                Constant::String(EcmaString::from_utf8("pending")),
                Constant::String(EcmaString::from_utf8("root-ran")),
                Constant::Int32(1),
            ],
            vec![entry_function(
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(4),
                    },
                    Instruction::StoreGlobal {
                        name: cid(3),
                        value: reg(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
            vec![
                Edge {
                    specifier: cid(1),
                    target: EdgeTarget::Local(ModuleId::new(0)),
                    kind: EdgeKind::Static,
                },
                Edge {
                    specifier: cid(2),
                    target: EdgeTarget::Local(ModuleId::new(1)),
                    kind: EdgeKind::Static,
                },
            ],
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![rejecting, pending, root], 2);

        let mut interpreter_host = SilentHost;
        let mut machine = Machine::new(&program, &mut interpreter_host, Limits::default());
        let error = machine
            .evaluate()
            .expect_err("interpreter propagates the first dependency rejection");
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                value,
                ..
            } if value == Value::int32(9)
        ));
        assert!(
            !machine
                .globals
                .contains_key(&EcmaString::from_utf8("root-ran")),
            "the root body must not run after a dependency rejects"
        );

        let mut reference_host = SilentHost;
        let error = NativeEngine::new(&program, &NoEntries, &mut reference_host, Limits::default())
            .run()
            .expect_err("reference backend propagates the async dependency rejection");
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                value,
                ..
            } if value == Value::int32(9)
        ));

        let entries = ForeignEntries {
            program_bytes: program.encode(),
            invoked: Cell::new(false),
        };
        let mut linked_host = SilentHost;
        let mut engine = NativeEngine::build(
            &program,
            &entries,
            &mut linked_host,
            Limits::default(),
            Backend::Linked,
        );
        let error = engine
            .run_linked()
            .expect_err("linked backend propagates the async dependency rejection");
        assert!(matches!(
            error,
            NativeError::Runtime(RuntimeError {
                kind: RuntimeErrorKind::UncaughtThrow {
                    value,
                    ..
                },
                ..
            }) if value == Value::int32(9)
        ));
        assert!(
            !entries.invoked.get(),
            "the rejected async graph must not run a linked entry"
        );
        assert!(
            !engine
                .machine
                .borrow()
                .globals
                .contains_key(&EcmaString::from_utf8("root-ran")),
            "the linked root body must not run after a dependency rejects"
        );
    }

    #[test]
    fn dynamic_import_cycle_matches_the_reference_backend() {
        let execution = assert_program_parity(&dynamic_cycle_program()).unwrap();
        assert_eq!(execution.value, Value::TRUE);
        assert_eq!(execution.entry_registers[0], execution.entry_registers[1]);
    }

    #[test]
    fn dynamic_import_throw_is_caught_at_the_requester_in_both_backends() {
        let root = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("./target"))],
            vec![Function::new(
                None,
                0,
                0,
                2,
                FunctionFlags::default(),
                vec![
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(1),
                    },
                    Instruction::Halt,
                    Instruction::Return { value: reg(1) },
                ],
                vec![ExceptionHandler {
                    start: pc(0),
                    end: pc(1),
                    handler: pc(2),
                    catch_register: reg(1),
                }],
            )],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            Vec::new(),
        );
        let target = program_module(
            "target",
            vec![Constant::Int32(9)],
            vec![entry_function(
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(1),
                    },
                    Instruction::Throw { value: reg(0) },
                ],
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(
            assert_program_parity(&linked(vec![root, target], 0))
                .unwrap()
                .value,
            Value::int32(9)
        );
    }

    #[test]
    fn linked_dynamic_import_invokes_the_target_once() {
        let program = dynamic_cycle_program();
        let entries = RecordingEntries {
            program_bytes: program.encode(),
            ..RecordingEntries::default()
        };
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine.machine.borrow_mut().instantiate_modules().unwrap();
        assert!(matches!(
            engine
                .machine
                .borrow_mut()
                .begin_module_evaluation(ModuleId::new(0))
                .unwrap(),
            crate::ModuleEvaluation::Ready(_)
        ));

        let mut registers = [Value::UNINITIALIZED; 3];
        engine.activations.borrow_mut().push(Activation {
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            pending_resume: None,
        });
        engine.push_native_roots(&registers);
        let handles = registers.as_mut_ptr();
        let mut shadow =
            ShadowFrame::new(std::ptr::null_mut(), 0, 0, handles, registers.len() as u16);
        let mut frame = NativeFrame::new(&mut shadow, &mut registers).unwrap();
        let first = engine.dispatch(&mut frame, HelperCall::Import { specifier: 1 });
        let second = engine.dispatch(&mut frame, HelperCall::Import { specifier: 1 });

        assert_eq!(first.tag, CompletionTag::Normal);
        assert_eq!(second.tag, CompletionTag::Normal);
        assert_eq!(first.value, second.value);
        assert_eq!(entries.invoked.borrow().as_slice(), &[(1, 0)]);
        engine.pop_native_roots();
        engine.activations.borrow_mut().pop();
    }

    #[test]
    fn native_functions_call_and_construct_with_engine_parity() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("Object")),
                Constant::String(EcmaString::from_utf8("prototype")),
                Constant::String(EcmaString::from_utf8("toString")),
                Constant::String(EcmaString::from_utf8("call")),
                Constant::String(EcmaString::from_utf8("[object Object]")),
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
    fn builtin_iterators_match_between_engines() {
        let module = verified(
            vec![Constant::Int32(8)],
            vec![entry_function(
                5,
                vec![
                    Instruction::CreateArray { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::ArrayPush {
                        array: reg(0),
                        value: reg(1),
                    },
                    Instruction::GetIterator {
                        dst: reg(2),
                        src: reg(0),
                        kind: IteratorKind::Sync,
                    },
                    Instruction::IteratorNext {
                        done: reg(3),
                        value: reg(4),
                        iterator: reg(2),
                    },
                    Instruction::Return { value: reg(4) },
                ],
            )],
        );

        assert_eq!(assert_parity(&module, || SilentHost), Value::int32(8));
    }

    #[test]
    fn bound_calls_match_between_engines() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("bind")),
                Constant::String(EcmaString::from_utf8("marker")),
                Constant::String(EcmaString::from_utf8("0")),
                Constant::String(EcmaString::from_utf8("1")),
                Constant::Int32(7),
                Constant::Int32(1),
                Constant::Int32(2),
            ],
            vec![
                entry_function(
                    8,
                    vec![
                        Instruction::CreateArray { dst: reg(0) },
                        Instruction::CreateClosure {
                            dst: reg(0),
                            function: FunctionId::new(1),
                            captures: reg(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(0),
                        },
                        Instruction::GetProperty {
                            dst: reg(2),
                            object: reg(0),
                            key: reg(1),
                        },
                        Instruction::CreateObject { dst: reg(3) },
                        Instruction::LoadConst {
                            dst: reg(4),
                            constant: cid(1),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(4),
                        },
                        Instruction::SetProperty {
                            object: reg(3),
                            key: reg(4),
                            value: reg(6),
                        },
                        Instruction::CreateArray { dst: reg(5) },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(5),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::Call {
                            dst: reg(7),
                            callee: reg(2),
                            this_value: reg(0),
                            arguments: reg(5),
                        },
                        Instruction::CreateArray { dst: reg(5) },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(6),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::Call {
                            dst: reg(7),
                            callee: reg(7),
                            this_value: reg(3),
                            arguments: reg(5),
                        },
                        Instruction::Return { value: reg(7) },
                    ],
                ),
                receiver_sum_function(),
            ],
        );

        assert_eq!(
            assert_parity(&module, || SilentHost),
            crate::number_value(10.0)
        );
    }

    #[test]
    fn applied_calls_match_between_engines() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("apply")),
                Constant::String(EcmaString::from_utf8("marker")),
                Constant::String(EcmaString::from_utf8("0")),
                Constant::String(EcmaString::from_utf8("1")),
                Constant::Int32(7),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::String(EcmaString::from_utf8("length")),
                Constant::Undefined,
            ],
            vec![
                entry_function(
                    8,
                    vec![
                        Instruction::CreateArray { dst: reg(0) },
                        Instruction::CreateClosure {
                            dst: reg(0),
                            function: FunctionId::new(1),
                            captures: reg(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(0),
                        },
                        Instruction::GetProperty {
                            dst: reg(2),
                            object: reg(0),
                            key: reg(1),
                        },
                        Instruction::CreateObject { dst: reg(3) },
                        Instruction::LoadConst {
                            dst: reg(4),
                            constant: cid(1),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(4),
                        },
                        Instruction::SetProperty {
                            object: reg(3),
                            key: reg(4),
                            value: reg(6),
                        },
                        Instruction::CreateArray { dst: reg(5) },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(5),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(6),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::CreateArray { dst: reg(6) },
                        Instruction::ArrayPush {
                            array: reg(6),
                            value: reg(3),
                        },
                        Instruction::ArrayPush {
                            array: reg(6),
                            value: reg(5),
                        },
                        Instruction::Call {
                            dst: reg(7),
                            callee: reg(2),
                            this_value: reg(0),
                            arguments: reg(6),
                        },
                        Instruction::CreateArray { dst: reg(0) },
                        Instruction::CreateClosure {
                            dst: reg(0),
                            function: FunctionId::new(2),
                            captures: reg(0),
                        },
                        Instruction::GetProperty {
                            dst: reg(2),
                            object: reg(0),
                            key: reg(1),
                        },
                        Instruction::CreateArray { dst: reg(6) },
                        Instruction::ArrayPush {
                            array: reg(6),
                            value: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(4),
                            constant: cid(8),
                        },
                        Instruction::ArrayPush {
                            array: reg(6),
                            value: reg(4),
                        },
                        Instruction::Call {
                            dst: reg(4),
                            callee: reg(2),
                            this_value: reg(0),
                            arguments: reg(6),
                        },
                        Instruction::Binary {
                            dst: reg(7),
                            op: BinaryOp::Add,
                            left: reg(7),
                            right: reg(4),
                        },
                        Instruction::Return { value: reg(7) },
                    ],
                ),
                receiver_sum_function(),
                module_function(
                    0,
                    3,
                    vec![
                        Instruction::LoadArguments { dst: reg(0) },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(7),
                        },
                        Instruction::GetProperty {
                            dst: reg(2),
                            object: reg(0),
                            key: reg(1),
                        },
                        Instruction::Return { value: reg(2) },
                    ],
                ),
            ],
        );

        assert_eq!(
            assert_parity(&module, || SilentHost),
            crate::number_value(10.0)
        );
    }

    #[test]
    fn bound_construction_matches_between_engines() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("bind")),
                Constant::String(EcmaString::from_utf8("prototype")),
                Constant::String(EcmaString::from_utf8("sum")),
                Constant::String(EcmaString::from_utf8("0")),
                Constant::String(EcmaString::from_utf8("1")),
                Constant::Int32(4),
                Constant::Int32(5),
                Constant::Int32(9),
                Constant::Undefined,
            ],
            vec![
                entry_function(
                    10,
                    vec![
                        Instruction::CreateArray { dst: reg(0) },
                        Instruction::CreateClosure {
                            dst: reg(0),
                            function: FunctionId::new(1),
                            captures: reg(0),
                        },
                        Instruction::CreateObject { dst: reg(2) },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(1),
                        },
                        Instruction::SetProperty {
                            object: reg(0),
                            key: reg(1),
                            value: reg(2),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(0),
                        },
                        Instruction::GetProperty {
                            dst: reg(3),
                            object: reg(0),
                            key: reg(1),
                        },
                        Instruction::CreateObject { dst: reg(4) },
                        Instruction::CreateArray { dst: reg(5) },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(4),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(5),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::Call {
                            dst: reg(7),
                            callee: reg(3),
                            this_value: reg(0),
                            arguments: reg(5),
                        },
                        Instruction::CreateArray { dst: reg(5) },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(6),
                        },
                        Instruction::ArrayPush {
                            array: reg(5),
                            value: reg(6),
                        },
                        Instruction::Construct {
                            dst: reg(8),
                            callee: reg(7),
                            arguments: reg(5),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(2),
                        },
                        Instruction::GetProperty {
                            dst: reg(9),
                            object: reg(8),
                            key: reg(1),
                        },
                        Instruction::LoadConst {
                            dst: reg(6),
                            constant: cid(7),
                        },
                        Instruction::Binary {
                            dst: reg(9),
                            op: BinaryOp::StrictEqual,
                            left: reg(9),
                            right: reg(6),
                        },
                        Instruction::Binary {
                            dst: reg(6),
                            op: BinaryOp::InstanceOf,
                            left: reg(8),
                            right: reg(7),
                        },
                        Instruction::Binary {
                            dst: reg(9),
                            op: BinaryOp::BitAnd,
                            left: reg(9),
                            right: reg(6),
                        },
                        Instruction::Return { value: reg(9) },
                    ],
                ),
                module_function(
                    0,
                    6,
                    vec![
                        Instruction::LoadThis { dst: reg(0) },
                        Instruction::LoadArguments { dst: reg(1) },
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(3),
                        },
                        Instruction::GetProperty {
                            dst: reg(3),
                            object: reg(1),
                            key: reg(2),
                        },
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(4),
                        },
                        Instruction::GetProperty {
                            dst: reg(4),
                            object: reg(1),
                            key: reg(2),
                        },
                        Instruction::Binary {
                            dst: reg(3),
                            op: BinaryOp::Add,
                            left: reg(3),
                            right: reg(4),
                        },
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(2),
                        },
                        Instruction::SetProperty {
                            object: reg(0),
                            key: reg(2),
                            value: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(5),
                            constant: cid(8),
                        },
                        Instruction::Return { value: reg(5) },
                    ],
                ),
            ],
        );

        assert_eq!(assert_parity(&module, || SilentHost), Value::int32(1));
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
    fn stale_pending_throw_cannot_replace_a_new_bytecode_throw() {
        let program = trivial_program();
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &NoEntries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine.pending_throw.set(Some(PendingThrow {
            value: Value::int32(1),
            origin: ThrowOrigin::ReferenceError { operation: "stale" },
        }));

        assert_eq!(
            engine.take_matching_throw(Value::int32(2)),
            (Value::int32(2), ThrowOrigin::Bytecode)
        );
        assert!(engine.pending_throw.get().is_none());
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

    #[test]
    fn linked_backend_routes_dynamic_targets_away_from_entry_table() {
        let root = one_module_program(&verified(
            Vec::new(),
            vec![entry_function(1, vec![Instruction::Halt])],
        ));
        let script = Arc::new(one_module_program(&verified(
            vec![Constant::Int32(42)],
            vec![entry_function(
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
            )],
        )));
        let entries = RecordingEntries::default();
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &root,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        engine.machine.borrow_mut().instantiate_modules().unwrap();
        let module = engine
            .machine
            .borrow_mut()
            .install_script_reserving(script, 0, 0)
            .unwrap();

        let outcome = engine.invoke_runtime(
            crate::RuntimeFunction {
                module,
                function: FunctionId::new(0),
            },
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
        );
        assert!(matches!(outcome, InvokeOutcome::Value(value) if value == Value::int32(42)));
        assert!(entries.invoked.borrow().is_empty());
    }
    fn generator_program(code: Vec<Instruction>, constants: Vec<Constant>) -> Program<Verified> {
        let generator = Function::new(
            None,
            0,
            0,
            3,
            FunctionFlags {
                is_async: false,
                is_generator: true,
            },
            code,
            Vec::new(),
        );
        one_module_program(&verified(
            constants,
            vec![entry_function(1, vec![Instruction::Halt]), generator],
        ))
    }

    fn yielding_generator_program() -> Program<Verified> {
        generator_program(
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::Suspend {
                    dst: reg(1),
                    src: reg(0),
                    resume: pc(2),
                },
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(1),
                },
                Instruction::Suspend {
                    dst: reg(1),
                    src: reg(0),
                    resume: pc(4),
                },
                Instruction::Return { value: reg(1) },
            ],
            vec![Constant::Int32(4), Constant::Int32(5)],
        )
    }

    fn invoke_test_generator<H: Host>(engine: &NativeEngine<'_, '_, H>) -> Value {
        match engine.invoke_runtime(
            crate::RuntimeFunction {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
            },
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
        ) {
            InvokeOutcome::Value(generator) => generator,
            _ => panic!("generator call must return its lazy generator object"),
        }
    }

    fn assert_iterator_result<H: Host>(
        engine: &NativeEngine<'_, '_, H>,
        outcome: InvokeOutcome,
        expected_value: Value,
        expected_done: bool,
    ) {
        let result = match outcome {
            InvokeOutcome::Value(result) => result,
            _ => panic!("generator next must return an iterator result"),
        };
        let mut machine = engine.machine.borrow_mut();
        let value = machine
            .get_named_property(result, "value")
            .expect("iterator result has value");
        let done = machine
            .get_named_property(result, "done")
            .expect("iterator result has done");
        assert_eq!(value, expected_value);
        assert_eq!(done, Value::boolean(expected_done));
    }

    #[test]
    fn reference_generator_is_lazy_yields_resumes_and_completes_stickily() {
        let program = yielding_generator_program();
        let mut host = SilentHost;
        let entries = NoEntries;
        let engine = NativeEngine::new(&program, &entries, &mut host, Limits::default());
        let generator = invoke_test_generator(&engine);

        {
            let machine = engine.machine.borrow();
            let index = machine.runtime_slot(generator).unwrap().unwrap();
            assert!(matches!(
                &machine.heap[index],
                HeapEntry::Generator {
                    state: GeneratorState::SuspendedStart(_),
                    ..
                }
            ));
        }

        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(999)),
            Value::int32(4),
            false,
        );
        {
            let machine = engine.machine.borrow();
            let index = machine.runtime_slot(generator).unwrap().unwrap();
            let HeapEntry::Generator {
                state: GeneratorState::Suspended(activation),
                ..
            } = &machine.heap[index]
            else {
                panic!("generator must retain its suspended activation");
            };
            assert_eq!(activation.resume_token, 2);
            assert_eq!(activation.registers[0], Value::int32(4));
        }

        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(99)),
            Value::int32(5),
            false,
        );
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(7)),
            Value::int32(7),
            true,
        );
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(8)),
            Value::UNDEFINED,
            true,
        );
        let machine = engine.machine.borrow();
        assert_eq!(machine.live_registers, 0);
        assert_eq!(machine.native_depth, 0);
    }

    #[test]
    fn reference_generator_throw_completes_and_preserves_value_and_origin() {
        let program = generator_program(
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::Throw { value: reg(0) },
            ],
            vec![Constant::Int32(42)],
        );
        let mut host = SilentHost;
        let entries = NoEntries;
        let engine = NativeEngine::new(&program, &entries, &mut host, Limits::default());
        let generator = invoke_test_generator(&engine);
        assert!(matches!(
            engine.resume_generator(generator, Value::UNDEFINED),
            InvokeOutcome::Threw(value, ThrowOrigin::Bytecode) if value == Value::int32(42)
        ));
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::UNDEFINED),
            Value::UNDEFINED,
            true,
        );
        let machine = engine.machine.borrow();
        assert_eq!(machine.live_registers, 0);
        assert_eq!(machine.native_depth, 0);
    }

    #[test]
    fn suspended_reference_generator_registers_enforce_the_global_ceiling() {
        let program = yielding_generator_program();
        let mut host = SilentHost;
        let entries = NoEntries;
        let limits = Limits {
            max_total_registers: 3,
            ..Limits::default()
        };
        let engine = NativeEngine::new(&program, &entries, &mut host, limits);
        let first = invoke_test_generator(&engine);
        let second = invoke_test_generator(&engine);
        assert_iterator_result(
            &engine,
            engine.resume_generator(first, Value::UNDEFINED),
            Value::int32(4),
            false,
        );
        assert!(matches!(
            engine.resume_generator(second, Value::UNDEFINED),
            InvokeOutcome::Fatal
        ));
        assert!(matches!(
            engine.pending_fatal_kind.take(),
            Some(RuntimeErrorKind::RegisterLimitExceeded { limit: 3 })
        ));
    }

    #[derive(Clone, Copy)]
    struct GeneratorEntryStep {
        token: u32,
        next_token: u32,
        tag: CompletionTag,
        value: Value,
    }

    struct GeneratorEntries {
        steps: Vec<GeneratorEntryStep>,
        call: Cell<usize>,
        handles: Cell<Option<*mut Value>>,
    }

    impl GeneratorEntries {
        fn yielding() -> Self {
            Self {
                steps: vec![
                    GeneratorEntryStep {
                        token: 0,
                        next_token: 2,
                        tag: CompletionTag::Suspend,
                        value: Value::int32(4),
                    },
                    GeneratorEntryStep {
                        token: 2,
                        next_token: 4,
                        tag: CompletionTag::Suspend,
                        value: Value::int32(5),
                    },
                    GeneratorEntryStep {
                        token: 4,
                        next_token: 4,
                        tag: CompletionTag::Normal,
                        value: Value::int32(7),
                    },
                ],
                call: Cell::new(0),
                handles: Cell::new(None),
            }
        }

        fn terminal(tag: CompletionTag, value: Value) -> Self {
            Self {
                steps: vec![GeneratorEntryStep {
                    token: 0,
                    next_token: 1,
                    tag,
                    value,
                }],
                call: Cell::new(0),
                handles: Cell::new(None),
            }
        }
    }

    impl NativeEntryTable for GeneratorEntries {
        fn program_bytes(&self) -> &[u8] {
            &[]
        }

        fn invoke(
            &self,
            module_id: u32,
            function_id: u32,
            frame: &mut ShadowFrame,
            out: &mut Completion,
        ) -> Result<CompletionTag, AbiError> {
            assert_eq!(module_id, 0);
            assert_eq!(function_id, 1);
            let call = self.call.get();
            let step = self.steps[call];
            self.call.set(call + 1);
            assert_eq!(frame.bytecode_pc, step.token);
            match self.handles.get() {
                Some(handles) => assert_eq!(
                    handles, frame.handles,
                    "linked resumes must reuse the saved register allocation"
                ),
                None => self.handles.set(Some(frame.handles)),
            }
            frame.bytecode_pc = step.next_token;
            *out = Completion::new(step.value);
            Ok(step.tag)
        }
    }

    #[test]
    fn linked_generator_reuses_saved_registers_and_dispatches_resume_tokens() {
        let program = yielding_generator_program();
        let entries = GeneratorEntries::yielding();
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        let generator = invoke_test_generator(&engine);
        assert_eq!(entries.call.get(), 0, "generator call is lazy");
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(999)),
            Value::int32(4),
            false,
        );
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(99)),
            Value::int32(5),
            false,
        );
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::int32(7)),
            Value::int32(7),
            true,
        );
        assert_iterator_result(
            &engine,
            engine.resume_generator(generator, Value::UNDEFINED),
            Value::UNDEFINED,
            true,
        );
        assert_eq!(entries.call.get(), 3);
        let machine = engine.machine.borrow();
        assert_eq!(machine.live_registers, 0);
        assert_eq!(machine.native_depth, 0);
    }

    #[test]
    fn linked_resume_value_dispatch_consumes_the_sent_value_once() {
        let program = yielding_generator_program();
        let entries = NoEntries;
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        let resumed_value = engine
            .machine
            .borrow_mut()
            .allocate(HeapEntry::String(EcmaString::from_utf8("resume")))
            .unwrap();
        engine.activations.borrow_mut().push(Activation {
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            pending_resume: Some(resumed_value),
        });
        let mut registers = vec![Value::UNINITIALIZED];
        engine.push_native_roots(&registers);
        engine.machine.borrow_mut().set_gc_watermarks_for_test(0, 0);
        let mut shadow = ShadowFrame::new(std::ptr::null_mut(), 2, 0, registers.as_mut_ptr(), 1);
        let mut frame = NativeFrame::new(&mut shadow, &mut registers).unwrap();
        let resumed = engine.dispatch(&mut frame, HelperCall::ResumeValue);
        assert_eq!(resumed, HelperResult::normal(resumed_value));
        assert!(
            engine
                .machine
                .borrow()
                .runtime_slot(resumed_value)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            engine.dispatch(&mut frame, HelperCall::ResumeValue).tag,
            CompletionTag::FatalTrap
        );
        engine.pop_native_roots();
        engine.activations.borrow_mut().pop();
    }

    #[test]
    fn linked_generator_throw_and_fatal_release_once_and_complete_stickily() {
        for (tag, expected_throw) in [
            (CompletionTag::Throw, true),
            (CompletionTag::FatalTrap, false),
        ] {
            let program = generator_program(vec![Instruction::Halt], Vec::new());
            let entries = GeneratorEntries::terminal(tag, Value::int32(42));
            let mut host = SilentHost;
            let engine = NativeEngine::build(
                &program,
                &entries,
                &mut host,
                Limits::default(),
                Backend::Linked,
            );
            let generator = invoke_test_generator(&engine);
            let outcome = engine.resume_generator(generator, Value::UNDEFINED);
            if expected_throw {
                assert!(matches!(
                    outcome,
                    InvokeOutcome::Threw(value, ThrowOrigin::Bytecode)
                        if value == Value::int32(42)
                ));
            } else {
                assert!(matches!(outcome, InvokeOutcome::Fatal));
            }
            assert_iterator_result(
                &engine,
                engine.resume_generator(generator, Value::UNDEFINED),
                Value::UNDEFINED,
                true,
            );
            let machine = engine.machine.borrow();
            assert_eq!(machine.live_registers, 0);
            assert_eq!(machine.native_depth, 0);
        }
    }

    #[test]
    fn linked_array_extend_drives_generator_through_linked_entries() {
        let program = yielding_generator_program();
        let entries = GeneratorEntries::yielding();
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        let generator = invoke_test_generator(&engine);
        let array = {
            let prototype = engine.machine.borrow().intrinsics.array_prototype;
            engine
                .machine
                .borrow_mut()
                .allocate(HeapEntry::Array {
                    elements: Vec::new(),
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                    extensible: true,
                    length_writable: true,
                })
                .unwrap()
        };
        assert!(matches!(
            engine.array_extend_active(array, generator),
            InvokeOutcome::Value(value) if value == Value::UNDEFINED
        ));
        assert_eq!(
            engine.machine.borrow().arguments_from_array(array).unwrap(),
            vec![Value::int32(4), Value::int32(5)]
        );
        assert_eq!(entries.call.get(), 3);
    }
    #[test]
    fn create_cell_helper_seeds_tdz_and_preserves_reference_error_origin() {
        let program = trivial_program();
        let entries = NoEntries;
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Reference,
        );
        let mut registers = vec![Value::UNINITIALIZED];
        engine.activations.borrow_mut().push(Activation {
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
            pending_resume: None,
        });
        engine.push_native_roots(&registers);
        let mut shadow = ShadowFrame::new(std::ptr::null_mut(), 0, 0, registers.as_mut_ptr(), 1);
        let mut frame = NativeFrame::new(&mut shadow, &mut registers).unwrap();
        let created = engine.dispatch(&mut frame, HelperCall::CreateCell);
        assert_eq!(created.tag, CompletionTag::Normal);
        let read = engine.dispatch(
            &mut frame,
            HelperCall::GetProperty {
                object: created.value,
                key: Value::int32(0),
            },
        );
        assert_eq!(read.tag, CompletionTag::Throw);
        assert!(matches!(
            engine.pending_throw.get(),
            Some(PendingThrow {
                origin: ThrowOrigin::ReferenceError { .. },
                ..
            })
        ));
        engine.pop_native_roots();
        engine.activations.borrow_mut().pop();
    }

    #[test]
    fn native_root_snapshot_retains_activation_values_through_collection() {
        let program = trivial_program();
        let entries = NoEntries;
        let mut host = SilentHost;
        let engine = NativeEngine::build(
            &program,
            &entries,
            &mut host,
            Limits::default(),
            Backend::Linked,
        );
        let values = {
            let mut machine = engine.machine.borrow_mut();
            [
                "register",
                "this",
                "new.target",
                "argument",
                "arguments",
                "resume",
                "throw",
            ]
            .map(|text| {
                machine
                    .allocate(HeapEntry::String(EcmaString::from_utf8(text)))
                    .unwrap()
            })
        };
        engine.activations.borrow_mut().push(Activation {
            this_value: values[1],
            new_target: values[2],
            args: vec![values[3]],
            arguments_object: Some(values[4]),
            pending_resume: Some(values[5]),
        });
        engine.pending_throw.set(Some(PendingThrow {
            value: values[6],
            origin: ThrowOrigin::Bytecode,
        }));
        let mut registers = [values[0]];
        engine.push_native_roots(&registers);
        let mut shadow = ShadowFrame::new(
            std::ptr::null_mut(),
            0,
            0,
            registers.as_mut_ptr(),
            registers.len() as u16,
        );
        let frame = NativeFrame::new(&mut shadow, &mut registers).unwrap();
        engine.machine.borrow_mut().set_gc_watermarks_for_test(0, 0);
        engine.prepare_native_helper(&frame);

        for value in values {
            assert!(
                engine
                    .machine
                    .borrow()
                    .runtime_slot(value)
                    .unwrap()
                    .is_some()
            );
        }
        engine.pop_native_roots();
        engine.activations.borrow_mut().pop();
        assert!(engine.machine.borrow().native_roots.is_empty());
    }
}
