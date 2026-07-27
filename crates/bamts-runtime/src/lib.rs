//! Deterministic register interpreter for verified production BamTS bytecode.
//!
//! Persisted constants never carry runtime identity. This interpreter therefore
//! owns a slot heap for strings, bigints, objects, arrays, and bytecode function
//! objects, and exposes only `bamts_native::Value` words at host boundaries.
//! Bytecode heap slots use segment 1; other segments remain host-owned.

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use bamts_bytecode::{
    BinaryOp, Constant, ConstantId, Function, FunctionId, Instruction, Module, Pc, UnaryOp,
    Verified,
};
use bamts_native::{Decoded, SlotId, Value};

const RUNTIME_HEAP_SEGMENT: u16 = 1;

/// The observable result of a terminated execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutcome {
    /// Reserved for output produced by host builtins.
    pub stdout: Vec<u8>,
    /// `0` for normal bytecode termination.
    pub exit_code: i32,
}

/// A terminated execution and the entry activation's observable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Execution {
    pub outcome: ExecutionOutcome,
    /// The explicit entry return value, or `undefined` for `Halt`.
    pub value: Value,
    /// Backward-compatible name for the final returned value.
    pub link: Value,
    pub entry_registers: Vec<Value>,
}

/// Deterministic execution and allocation ceilings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    pub fuel: u64,
    pub max_call_depth: usize,
    pub max_total_registers: usize,
    pub max_argument_count: u32,
    pub max_heap_slots: usize,
    /// Cumulative bytes admitted for heap payloads and property/element growth.
    pub max_heap_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000,
            max_call_depth: 64,
            max_total_registers: 1 << 20,
            max_argument_count: 1 << 16,
            max_heap_slots: 1 << 20,
            max_heap_bytes: 64 << 20,
        }
    }
}

/// A JavaScript throw raised by the embedding host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostThrow {
    pub value: Value,
}

/// External operations. Segment-1 heap values belong to this runtime and may be
/// passed to or echoed by the host, but the host must not forge new segment-1
/// slot ids. Default methods fail by throwing `undefined`, never by succeeding
/// with placeholder behavior.
pub trait Host {
    fn property_get(&mut self, _object: Value, _key: &str) -> Result<Value, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn property_set(&mut self, _object: Value, _key: &str, _value: Value) -> Result<(), HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn property_delete(&mut self, _object: Value, _key: &str) -> Result<bool, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn property_has(&mut self, _object: Value, _key: &str) -> Result<bool, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn call(
        &mut self,
        _callee: Value,
        _this: Value,
        _arguments: &[Value],
    ) -> Result<Value, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn construct(&mut self, _callee: Value, _arguments: &[Value]) -> Result<Value, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn instance_of(&mut self, _value: Value, _constructor: Value) -> Result<bool, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    /// Resolve one explicit `Suspend`, receiving the yielded value and returning
    /// the value written to its destination register.
    fn awaited(&mut self, _value: Value) -> Result<Value, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }

    fn import(&mut self, _specifier: &str) -> Result<Value, HostThrow> {
        Err(HostThrow {
            value: Value::UNDEFINED,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThrowOrigin {
    Bytecode,
    Host,
    TypeError { operation: &'static str },
    RangeError { operation: &'static str },
}

/// Source metadata attached to every machine failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSource {
    pub function_name: Option<String>,
    pub instruction: Instruction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub function: FunctionId,
    pub pc: Pc,
    pub source: RuntimeSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeErrorKind {
    UncaughtThrow { value: Value, origin: ThrowOrigin },
    FuelExhausted { limit: u64 },
    CallDepthExceeded { limit: usize },
    RegisterLimitExceeded { limit: usize },
    ArgumentLimitExceeded { limit: u32, requested: u32 },
    HeapSlotLimitExceeded { limit: usize },
    HeapByteLimitExceeded { limit: usize },
    InvalidHostValue { value: Value },
    InvalidRuntimeHeapReference { slot: u32 },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runtime error in function {} at pc {}",
            self.function.get(),
            self.pc.get()
        )?;
        if let Some(name) = &self.source.function_name {
            write!(formatter, " ({name})")?;
        }
        write!(formatter, ": ")?;
        match &self.kind {
            RuntimeErrorKind::UncaughtThrow { value, origin } => write!(
                formatter,
                "uncaught {origin:?} throw (value {:#018x})",
                value.to_bits()
            ),
            RuntimeErrorKind::FuelExhausted { limit } => {
                write!(formatter, "fuel exhausted after {limit} instructions")
            }
            RuntimeErrorKind::CallDepthExceeded { limit } => {
                write!(formatter, "call depth limit {limit} exceeded")
            }
            RuntimeErrorKind::RegisterLimitExceeded { limit } => {
                write!(formatter, "live register limit {limit} exceeded")
            }
            RuntimeErrorKind::ArgumentLimitExceeded { limit, requested } => write!(
                formatter,
                "argument count {requested} exceeds runtime limit {limit}"
            ),
            RuntimeErrorKind::HeapSlotLimitExceeded { limit } => {
                write!(formatter, "heap slot limit {limit} exceeded")
            }
            RuntimeErrorKind::HeapByteLimitExceeded { limit } => {
                write!(formatter, "heap byte limit {limit} exceeded")
            }
            RuntimeErrorKind::InvalidHostValue { value } => write!(
                formatter,
                "host returned malformed or forged value {:#018x}",
                value.to_bits()
            ),
            RuntimeErrorKind::InvalidRuntimeHeapReference { slot } => {
                write!(formatter, "runtime heap slot {slot} does not exist")
            }
        }
    }
}

impl Error for RuntimeError {}

/// Converts constants that need no runtime identity to ABI values. Strings and
/// bigints are materialized by `LoadConst` in the machine's slot heap.
#[must_use]
pub fn constant_value(constant: &Constant) -> Option<Value> {
    match constant {
        Constant::Number(bits) => Some(Value::number(bits.to_f64())),
        Constant::Int32(value) => Some(Value::int32(*value as u32)),
        Constant::String(_) | Constant::BigInt(_) => None,
        Constant::Boolean(value) => Some(Value::boolean(*value)),
        Constant::Null => Some(Value::NULL),
        Constant::Undefined => Some(Value::UNDEFINED),
    }
}

#[derive(Clone, Debug)]
enum HeapEntry {
    String(String),
    BigInt(String),
    Object {
        properties: BTreeMap<String, Value>,
        constructor: Option<Value>,
    },
    Array {
        elements: Vec<Value>,
        properties: BTreeMap<String, Value>,
        constructor: Option<Value>,
    },
    Function {
        function: FunctionId,
        properties: BTreeMap<String, Value>,
    },
}

impl HeapEntry {
    fn initial_bytes(&self) -> usize {
        match self {
            Self::String(text) | Self::BigInt(text) => text.len(),
            Self::Object { .. } | Self::Array { .. } | Self::Function { .. } => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ReturnTo {
    destination: usize,
    call_pc: usize,
    constructed: Option<Value>,
}

#[derive(Clone, Debug)]
struct Frame {
    function: usize,
    pc: usize,
    registers: Vec<Value>,
    return_to: Option<ReturnTo>,
}

impl Frame {
    fn new(
        function: usize,
        metadata: &Function,
        arguments: &[Value],
        return_to: Option<ReturnTo>,
    ) -> Self {
        let mut registers = vec![Value::UNINITIALIZED; metadata.register_count() as usize];
        for (index, slot) in registers
            .iter_mut()
            .take(metadata.parameter_count() as usize)
            .enumerate()
        {
            *slot = arguments.get(index).copied().unwrap_or(Value::UNDEFINED);
        }
        Self {
            function,
            pc: 0,
            registers,
            return_to,
        }
    }
}

#[derive(Clone, Debug)]
enum EvalFailure {
    Throw(ThrowOrigin),
    Runtime(RuntimeErrorKind),
}

#[derive(Clone, Debug)]
enum PropertyResult {
    Value(Value),
    Text(String),
}

/// The production bytecode interpreter.
pub struct Machine<'a, H: Host> {
    module: &'a Module<Verified>,
    host: &'a mut H,
    limits: Limits,
    frames: Vec<Frame>,
    heap: Vec<HeapEntry>,
    heap_bytes: usize,
    live_registers: usize,
    fuel: u64,
}

pub fn run<H: Host>(
    module: &Module<Verified>,
    host: &mut H,
    limits: &Limits,
) -> Result<ExecutionOutcome, RuntimeError> {
    Machine::new(module, host, limits.clone())
        .run()
        .map(|execution| execution.outcome)
}

impl<'a, H: Host> Machine<'a, H> {
    #[must_use]
    pub fn new(module: &'a Module<Verified>, host: &'a mut H, limits: Limits) -> Self {
        let entry = module.entry().get() as usize;
        let frame = Frame::new(entry, &module.functions()[entry], &[], None);
        let live_registers = frame.registers.len();
        Self {
            fuel: limits.fuel,
            module,
            host,
            limits,
            frames: vec![frame],
            heap: Vec::new(),
            heap_bytes: 0,
            live_registers,
        }
    }

    pub fn run(mut self) -> Result<Execution, RuntimeError> {
        if self.frames.len() > self.limits.max_call_depth {
            return Err(self.error_here(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            }));
        }
        if self.live_registers > self.limits.max_total_registers {
            return Err(self.error_here(RuntimeErrorKind::RegisterLimitExceeded {
                limit: self.limits.max_total_registers,
            }));
        }

        loop {
            let frame_index = self.frames.len() - 1;
            let (function_index, pc) = {
                let frame = &self.frames[frame_index];
                (frame.function, frame.pc)
            };
            if self.fuel == 0 {
                return Err(self.error_at(
                    RuntimeErrorKind::FuelExhausted {
                        limit: self.limits.fuel,
                    },
                    function_index,
                    pc,
                ));
            }
            self.fuel -= 1;
            let instruction = self.module.functions()[function_index].code()[pc];

            match instruction {
                Instruction::LoadConst { dst, constant } => {
                    let value = self.load_constant(constant, function_index, pc)?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::Move { dst, src } => {
                    let value = self.read_register(frame_index, src.get());
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::Unary { dst, op, operand } => {
                    let value = self.read_register(frame_index, operand.get());
                    match self.eval_unary(op, value) {
                        Ok(result) => {
                            self.write_register(frame_index, dst.get(), result);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::Binary {
                    dst,
                    op,
                    left,
                    right,
                } => {
                    let left = self.read_register(frame_index, left.get());
                    let right = self.read_register(frame_index, right.get());
                    match self.eval_binary(op, left, right) {
                        Ok(result) => {
                            self.write_register(frame_index, dst.get(), result);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::CreateObject { dst } => {
                    let value = self
                        .allocate(HeapEntry::Object {
                            properties: BTreeMap::new(),
                            constructor: None,
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::CreateArray { dst } => {
                    let value = self
                        .allocate(HeapEntry::Array {
                            elements: Vec::new(),
                            properties: BTreeMap::new(),
                            constructor: None,
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::DefineFunction { dst, function } => {
                    let value = self
                        .allocate(HeapEntry::Function {
                            function,
                            properties: BTreeMap::new(),
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::GetProperty { dst, object, key } => {
                    let object = self.read_register(frame_index, object.get());
                    let key = self.constant_string(key).to_owned();
                    match self.get_property(object, &key) {
                        Ok(PropertyResult::Value(value)) => {
                            self.validate_host_value(value)
                                .map_err(|kind| self.error_at(kind, function_index, pc))?;
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Ok(PropertyResult::Text(text)) => {
                            let value = self
                                .allocate(HeapEntry::String(text))
                                .map_err(|kind| self.error_at(kind, function_index, pc))?;
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::SetProperty { object, key, value } => {
                    let object = self.read_register(frame_index, object.get());
                    let value = self.read_register(frame_index, value.get());
                    let key = self.constant_string(key).to_owned();
                    match self.set_property(object, key, value) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::DeleteProperty { dst, object, key } => {
                    let object = self.read_register(frame_index, object.get());
                    let key = self.constant_string(key).to_owned();
                    match self.delete_property(object, &key) {
                        Ok(deleted) => {
                            self.write_register(frame_index, dst.get(), Value::boolean(deleted));
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::Call {
                    dst,
                    callee,
                    this_value,
                    args_start,
                    arg_count,
                } => {
                    let callee = self.read_register(frame_index, callee.get());
                    let this_value = self.read_register(frame_index, this_value.get());
                    let arguments =
                        self.argument_window(frame_index, args_start.get(), arg_count, pc)?;
                    self.frames[frame_index].pc = pc + 1;
                    self.execute_call(callee, this_value, &arguments, dst.get(), pc, None)?;
                }
                Instruction::Construct {
                    dst,
                    callee,
                    args_start,
                    arg_count,
                } => {
                    let callee = self.read_register(frame_index, callee.get());
                    let arguments =
                        self.argument_window(frame_index, args_start.get(), arg_count, pc)?;
                    self.frames[frame_index].pc = pc + 1;
                    self.execute_construct(callee, &arguments, dst.get(), pc)?;
                }
                Instruction::Jump { target } => {
                    self.frames[frame_index].pc = target.get() as usize;
                }
                Instruction::JumpIfTrue { condition, target } => {
                    let condition = self.read_register(frame_index, condition.get());
                    self.frames[frame_index].pc = if self.truthy(condition) {
                        target.get() as usize
                    } else {
                        pc + 1
                    };
                }
                Instruction::JumpIfFalse { condition, target } => {
                    let condition = self.read_register(frame_index, condition.get());
                    self.frames[frame_index].pc = if self.truthy(condition) {
                        pc + 1
                    } else {
                        target.get() as usize
                    };
                }
                Instruction::Return { value } => {
                    let value = self.read_register(frame_index, value.get());
                    if let Some(execution) = self.complete_frame(value) {
                        return Ok(execution);
                    }
                }
                Instruction::Throw { value } => {
                    let value = self.read_register(frame_index, value.get());
                    self.throw(value, ThrowOrigin::Bytecode, pc)?;
                }
                Instruction::Suspend { dst, src, resume } => {
                    let yielded = self.read_register(frame_index, src.get());
                    match self.host.awaited(yielded) {
                        Ok(value) => {
                            self.validate_host_value(value)
                                .map_err(|kind| self.error_at(kind, function_index, pc))?;
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = resume.get() as usize;
                        }
                        Err(thrown) => self.host_throw(thrown, pc)?,
                    }
                }
                Instruction::Import { dst, specifier } => {
                    let specifier = self.constant_string(specifier).to_owned();
                    match self.host.import(&specifier) {
                        Ok(value) => {
                            self.validate_host_value(value)
                                .map_err(|kind| self.error_at(kind, function_index, pc))?;
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(thrown) => self.host_throw(thrown, pc)?,
                    }
                }
                Instruction::Halt => {
                    if let Some(execution) = self.complete_frame(Value::UNDEFINED) {
                        return Ok(execution);
                    }
                }
            }
        }
    }

    fn read_register(&self, frame: usize, register: u32) -> Value {
        self.frames[frame].registers[register as usize]
    }

    fn write_register(&mut self, frame: usize, register: u32, value: Value) {
        self.frames[frame].registers[register as usize] = value;
    }

    fn constant_string(&self, id: ConstantId) -> &str {
        match &self.module.constants()[id.get() as usize] {
            Constant::String(text) => text,
            _ => unreachable!("verified property/import keys are strings"),
        }
    }

    fn load_constant(
        &mut self,
        id: ConstantId,
        function: usize,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        match &self.module.constants()[id.get() as usize] {
            Constant::String(text) => self
                .allocate(HeapEntry::String(text.clone()))
                .map_err(|kind| self.error_at(kind, function, pc)),
            Constant::BigInt(value) => self
                .allocate(HeapEntry::BigInt(value.as_str().to_owned()))
                .map_err(|kind| self.error_at(kind, function, pc)),
            constant => Ok(constant_value(constant).expect("non-heap constant")),
        }
    }

    fn allocate(&mut self, entry: HeapEntry) -> Result<Value, RuntimeErrorKind> {
        if self.heap.len() >= self.limits.max_heap_slots || self.heap.len() == u32::MAX as usize {
            return Err(RuntimeErrorKind::HeapSlotLimitExceeded {
                limit: self.limits.max_heap_slots,
            });
        }
        self.charge_heap(entry.initial_bytes())?;
        let slot = self.heap.len() as u32 + 1;
        self.heap.push(entry);
        let id = SlotId::from_parts(RUNTIME_HEAP_SEGMENT, slot)
            .expect("runtime segment and one-based slot are nonzero");
        Ok(Value::heap_ref(id))
    }

    fn charge_heap(&mut self, bytes: usize) -> Result<(), RuntimeErrorKind> {
        let Some(total) = self.heap_bytes.checked_add(bytes) else {
            return Err(RuntimeErrorKind::HeapByteLimitExceeded {
                limit: self.limits.max_heap_bytes,
            });
        };
        if total > self.limits.max_heap_bytes {
            return Err(RuntimeErrorKind::HeapByteLimitExceeded {
                limit: self.limits.max_heap_bytes,
            });
        }
        self.heap_bytes = total;
        Ok(())
    }

    fn runtime_slot(&self, value: Value) -> Result<Option<usize>, RuntimeErrorKind> {
        let Some(decoded) = value.decode() else {
            return Err(RuntimeErrorKind::InvalidHostValue { value });
        };
        let Decoded::HeapRef(id) = decoded else {
            return Ok(None);
        };
        if id.segment() != RUNTIME_HEAP_SEGMENT {
            return Ok(None);
        }
        let index = id.slot() as usize - 1;
        if index >= self.heap.len() {
            return Err(RuntimeErrorKind::InvalidRuntimeHeapReference { slot: id.slot() });
        }
        Ok(Some(index))
    }

    fn validate_host_value(&self, value: Value) -> Result<(), RuntimeErrorKind> {
        self.runtime_slot(value).map(|_| ())
    }

    fn argument_window(
        &self,
        frame: usize,
        start: u32,
        count: u32,
        pc: usize,
    ) -> Result<Vec<Value>, RuntimeError> {
        if count > self.limits.max_argument_count {
            return Err(self.error_here_at(
                RuntimeErrorKind::ArgumentLimitExceeded {
                    limit: self.limits.max_argument_count,
                    requested: count,
                },
                pc,
            ));
        }
        let start = start as usize;
        let end = start + count as usize;
        Ok(self.frames[frame].registers[start..end].to_vec())
    }

    fn push_frame(
        &mut self,
        function: FunctionId,
        arguments: &[Value],
        return_to: ReturnTo,
        call_pc: usize,
    ) -> Result<(), RuntimeError> {
        let function_index = function.get() as usize;
        let metadata = &self.module.functions()[function_index];
        if self.frames.len() >= self.limits.max_call_depth {
            return Err(self.error_here_at(
                RuntimeErrorKind::CallDepthExceeded {
                    limit: self.limits.max_call_depth,
                },
                call_pc,
            ));
        }
        let next_registers = metadata.register_count() as usize;
        if self.live_registers.saturating_add(next_registers) > self.limits.max_total_registers {
            return Err(self.error_here_at(
                RuntimeErrorKind::RegisterLimitExceeded {
                    limit: self.limits.max_total_registers,
                },
                call_pc,
            ));
        }
        let frame = Frame::new(function_index, metadata, arguments, Some(return_to));
        self.live_registers += next_registers;
        self.frames.push(frame);
        Ok(())
    }

    fn execute_call(
        &mut self,
        callee: Value,
        this_value: Value,
        arguments: &[Value],
        destination: u32,
        call_pc: usize,
        constructed: Option<Value>,
    ) -> Result<(), RuntimeError> {
        match self.runtime_slot(callee) {
            Ok(Some(index)) => {
                let HeapEntry::Function { function, .. } = self.heap[index] else {
                    return self.throw_type("call", call_pc);
                };
                self.push_frame(
                    function,
                    arguments,
                    ReturnTo {
                        destination: destination as usize,
                        call_pc,
                        constructed,
                    },
                    call_pc,
                )
            }
            Ok(None) => match callee.decode() {
                Some(Decoded::HeapRef(_)) => match self.host.call(callee, this_value, arguments) {
                    Ok(value) => {
                        self.validate_host_value(value)
                            .map_err(|kind| self.error_here_at(kind, call_pc))?;
                        self.write_register(self.frames.len() - 1, destination, value);
                        Ok(())
                    }
                    Err(thrown) => self.host_throw(thrown, call_pc),
                },
                _ => self.throw_type("call", call_pc),
            },
            Err(kind) => Err(self.error_here_at(kind, call_pc)),
        }
    }

    fn execute_construct(
        &mut self,
        callee: Value,
        arguments: &[Value],
        destination: u32,
        call_pc: usize,
    ) -> Result<(), RuntimeError> {
        match self.runtime_slot(callee) {
            Ok(Some(index)) => {
                let HeapEntry::Function { .. } = self.heap[index] else {
                    return self.throw_type("construct", call_pc);
                };
                let object = self
                    .allocate(HeapEntry::Object {
                        properties: BTreeMap::new(),
                        constructor: Some(callee),
                    })
                    .map_err(|kind| self.error_here_at(kind, call_pc))?;
                self.execute_call(
                    callee,
                    object,
                    arguments,
                    destination,
                    call_pc,
                    Some(object),
                )
            }
            Ok(None) => match callee.decode() {
                Some(Decoded::HeapRef(_)) => match self.host.construct(callee, arguments) {
                    Ok(value) => {
                        self.validate_host_value(value)
                            .map_err(|kind| self.error_here_at(kind, call_pc))?;
                        self.write_register(self.frames.len() - 1, destination, value);
                        Ok(())
                    }
                    Err(thrown) => self.host_throw(thrown, call_pc),
                },
                _ => self.throw_type("construct", call_pc),
            },
            Err(kind) => Err(self.error_here_at(kind, call_pc)),
        }
    }

    fn complete_frame(&mut self, returned: Value) -> Option<Execution> {
        let frame = self.frames.pop().expect("an activation is executing");
        self.live_registers -= frame.registers.len();
        match frame.return_to {
            None => {
                let outcome = ExecutionOutcome {
                    stdout: Vec::new(),
                    exit_code: 0,
                };
                Some(Execution {
                    outcome,
                    value: returned,
                    link: returned,
                    entry_registers: frame.registers,
                })
            }
            Some(return_to) => {
                let value = match return_to.constructed {
                    Some(object) if !self.is_object(returned) => object,
                    _ => returned,
                };
                self.frames.last_mut().expect("callee has caller").registers
                    [return_to.destination] = value;
                None
            }
        }
    }

    fn resolve_failure(&mut self, failure: EvalFailure, pc: usize) -> Result<(), RuntimeError> {
        match failure {
            EvalFailure::Throw(origin) => self.throw(Value::UNDEFINED, origin, pc),
            EvalFailure::Runtime(kind) => Err(self.error_here_at(kind, pc)),
        }
    }

    fn throw_type(&mut self, operation: &'static str, pc: usize) -> Result<(), RuntimeError> {
        self.throw(Value::UNDEFINED, ThrowOrigin::TypeError { operation }, pc)
    }

    fn host_throw(&mut self, thrown: HostThrow, pc: usize) -> Result<(), RuntimeError> {
        self.validate_host_value(thrown.value)
            .map_err(|kind| self.error_here_at(kind, pc))?;
        self.throw(thrown.value, ThrowOrigin::Host, pc)
    }

    fn throw(
        &mut self,
        value: Value,
        origin: ThrowOrigin,
        faulting_pc: usize,
    ) -> Result<(), RuntimeError> {
        let site_function = self
            .frames
            .last()
            .expect("an activation is executing")
            .function;
        let mut search_pc = faulting_pc;
        loop {
            let frame_index = self.frames.len() - 1;
            let function_index = self.frames[frame_index].function;
            let function = &self.module.functions()[function_index];
            if let Some(handler) = innermost_handler(function, search_pc) {
                let frame = &mut self.frames[frame_index];
                frame.registers[handler.catch_register.get() as usize] = value;
                frame.pc = handler.handler.get() as usize;
                return Ok(());
            }
            let frame = self.frames.pop().expect("throw walks live frames");
            self.live_registers -= frame.registers.len();
            match frame.return_to {
                Some(return_to) => search_pc = return_to.call_pc,
                None => {
                    return Err(self.error_at(
                        RuntimeErrorKind::UncaughtThrow { value, origin },
                        site_function,
                        faulting_pc,
                    ));
                }
            }
        }
    }

    fn error_here(&self, kind: RuntimeErrorKind) -> RuntimeError {
        let frame = self.frames.last().expect("an activation is executing");
        self.error_at(kind, frame.function, frame.pc)
    }

    fn error_here_at(&self, kind: RuntimeErrorKind, pc: usize) -> RuntimeError {
        let function = self
            .frames
            .last()
            .expect("an activation is executing")
            .function;
        self.error_at(kind, function, pc)
    }

    fn error_at(&self, kind: RuntimeErrorKind, function: usize, pc: usize) -> RuntimeError {
        let metadata = &self.module.functions()[function];
        let function_name =
            metadata
                .name()
                .and_then(|id| match &self.module.constants()[id.get() as usize] {
                    Constant::String(name) => Some(name.clone()),
                    _ => None,
                });
        RuntimeError {
            kind,
            function: FunctionId::new(function as u32),
            pc: Pc::new(pc as u32),
            source: RuntimeSource {
                function_name,
                instruction: metadata.code()[pc],
            },
        }
    }

    fn get_property(&mut self, object: Value, key: &str) -> Result<PropertyResult, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::String(text) => {
                    if key == "length" {
                        return Ok(PropertyResult::Value(number_value(
                            text.chars().count() as f64
                        )));
                    }
                    if let Some(index) = array_index(key) {
                        return Ok(PropertyResult::Text(
                            text.chars()
                                .nth(index as usize)
                                .map_or_else(String::new, |ch| ch.to_string()),
                        ));
                    }
                    Ok(PropertyResult::Value(Value::UNDEFINED))
                }
                HeapEntry::BigInt(_) => Ok(PropertyResult::Value(Value::UNDEFINED)),
                HeapEntry::Object { properties, .. } => Ok(PropertyResult::Value(
                    properties.get(key).copied().unwrap_or(Value::UNDEFINED),
                )),
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    if key == "length" {
                        Ok(PropertyResult::Value(number_value(elements.len() as f64)))
                    } else if let Some(index) = array_index(key) {
                        Ok(PropertyResult::Value(
                            elements
                                .get(index as usize)
                                .copied()
                                .filter(|value| *value != Value::HOLE)
                                .unwrap_or(Value::UNDEFINED),
                        ))
                    } else {
                        Ok(PropertyResult::Value(
                            properties.get(key).copied().unwrap_or(Value::UNDEFINED),
                        ))
                    }
                }
                HeapEntry::Function {
                    function,
                    properties,
                } => {
                    if let Some(value) = properties.get(key) {
                        return Ok(PropertyResult::Value(*value));
                    }
                    let metadata = &self.module.functions()[function.get() as usize];
                    match key {
                        "length" => Ok(PropertyResult::Value(number_value(
                            metadata.parameter_count() as f64,
                        ))),
                        "name" => Ok(PropertyResult::Text(
                            metadata
                                .name()
                                .map(|id| self.constant_string(id).to_owned())
                                .unwrap_or_default(),
                        )),
                        _ => Ok(PropertyResult::Value(Value::UNDEFINED)),
                    }
                }
            },
            None => match object.decode() {
                Some(Decoded::HeapRef(_)) => self
                    .host
                    .property_get(object, key)
                    .map(PropertyResult::Value)
                    .map_err(|_| EvalFailure::Throw(ThrowOrigin::Host)),
                Some(_) => Ok(PropertyResult::Value(Value::UNDEFINED)),
                None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                    value: object,
                })),
            },
        }
    }

    fn set_property(
        &mut self,
        object: Value,
        key: String,
        value: Value,
    ) -> Result<(), EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                let growth = match &self.heap[index] {
                    HeapEntry::Object { properties, .. }
                    | HeapEntry::Function { properties, .. } => {
                        usize::from(!properties.contains_key(&key)) * (key.len() + 8)
                    }
                    HeapEntry::Array {
                        elements,
                        properties,
                        ..
                    } => {
                        if key == "length" {
                            0
                        } else if let Some(array_index) = array_index(&key) {
                            (array_index as usize + 1).saturating_sub(elements.len()) * 8
                        } else {
                            usize::from(!properties.contains_key(&key)) * (key.len() + 8)
                        }
                    }
                    HeapEntry::String(_) | HeapEntry::BigInt(_) => {
                        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                            operation: "set property on primitive",
                        }));
                    }
                };
                self.charge_heap(growth).map_err(EvalFailure::Runtime)?;
                match &mut self.heap[index] {
                    HeapEntry::Object { properties, .. }
                    | HeapEntry::Function { properties, .. } => {
                        properties.insert(key, value);
                    }
                    HeapEntry::Array {
                        elements,
                        properties,
                        ..
                    } => {
                        if key == "length" {
                            let length = exact_array_length(value).ok_or(EvalFailure::Throw(
                                ThrowOrigin::RangeError {
                                    operation: "set array length",
                                },
                            ))?;
                            elements.resize(length, Value::HOLE);
                        } else if let Some(array_index) = array_index(&key) {
                            let index = array_index as usize;
                            if elements.len() <= index {
                                elements.resize(index + 1, Value::HOLE);
                            }
                            elements[index] = value;
                        } else {
                            properties.insert(key, value);
                        }
                    }
                    HeapEntry::String(_) | HeapEntry::BigInt(_) => unreachable!(),
                }
                Ok(())
            }
            None => match object.decode() {
                Some(Decoded::HeapRef(_)) => self
                    .host
                    .property_set(object, &key, value)
                    .map_err(|_| EvalFailure::Throw(ThrowOrigin::Host)),
                Some(_) => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "set property on primitive",
                })),
                None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                    value: object,
                })),
            },
        }
    }

    fn delete_property(&mut self, object: Value, key: &str) -> Result<bool, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => match &mut self.heap[index] {
                HeapEntry::Object { properties, .. } | HeapEntry::Function { properties, .. } => {
                    properties.remove(key);
                    Ok(true)
                }
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    if key == "length" {
                        Ok(false)
                    } else if let Some(index) = array_index(key) {
                        if let Some(element) = elements.get_mut(index as usize) {
                            *element = Value::HOLE;
                        }
                        Ok(true)
                    } else {
                        properties.remove(key);
                        Ok(true)
                    }
                }
                HeapEntry::String(_) | HeapEntry::BigInt(_) => Ok(true),
            },
            None => match object.decode() {
                Some(Decoded::HeapRef(_)) => self
                    .host
                    .property_delete(object, key)
                    .map_err(|_| EvalFailure::Throw(ThrowOrigin::Host)),
                Some(_) => Ok(true),
                None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                    value: object,
                })),
            },
        }
    }

    fn has_property(&mut self, object: Value, key: &str) -> Result<bool, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::String(text) => Ok(key == "length"
                    || array_index(key).is_some_and(|i| (i as usize) < text.chars().count())),
                HeapEntry::BigInt(_) => Ok(false),
                HeapEntry::Object { properties, .. } => Ok(properties.contains_key(key)),
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => Ok(key == "length"
                    || array_index(key).is_some_and(|i| {
                        elements.get(i as usize).is_some_and(|v| *v != Value::HOLE)
                    })
                    || properties.contains_key(key)),
                HeapEntry::Function { properties, .. } => {
                    Ok(key == "name" || key == "length" || properties.contains_key(key))
                }
            },
            None => match object.decode() {
                Some(Decoded::HeapRef(_)) => self
                    .host
                    .property_has(object, key)
                    .map_err(|_| EvalFailure::Throw(ThrowOrigin::Host)),
                Some(_) => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "in",
                })),
                None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                    value: object,
                })),
            },
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, operand: Value) -> Result<Value, EvalFailure> {
        match op {
            UnaryOp::Void => Ok(Value::UNDEFINED),
            UnaryOp::TypeOf => {
                let text = self.type_of(operand).to_owned();
                self.allocate(HeapEntry::String(text))
                    .map_err(EvalFailure::Runtime)
            }
            UnaryOp::Plus => self.to_number(operand),
            UnaryOp::Negate => {
                if let Some(text) = self.bigint_text(operand) {
                    let negated = if text == "0" {
                        "0".to_owned()
                    } else if let Some(positive) = text.strip_prefix('-') {
                        positive.to_owned()
                    } else {
                        format!("-{text}")
                    };
                    return self
                        .allocate(HeapEntry::BigInt(negated))
                        .map_err(EvalFailure::Runtime);
                }
                let number =
                    numeric_f64(self.to_number(operand)?).expect("ToNumber returns numeric");
                Ok(number_value(-number))
            }
            UnaryOp::BitwiseNot => {
                if let Some(text) = self.bigint_text(operand) {
                    let value = text.parse::<i128>().map_err(|_| {
                        EvalFailure::Throw(ThrowOrigin::RangeError {
                            operation: "bigint bitwise not",
                        })
                    })?;
                    return self
                        .allocate(HeapEntry::BigInt((!value).to_string()))
                        .map_err(EvalFailure::Runtime);
                }
                Ok(Value::int32(
                    (!to_int32(numeric_f64(self.to_number(operand)?).unwrap())) as u32,
                ))
            }
            UnaryOp::LogicalNot => Ok(Value::boolean(!self.truthy(operand))),
        }
    }

    fn eval_binary(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
    ) -> Result<Value, EvalFailure> {
        match op {
            BinaryOp::StrictEqual => Ok(Value::boolean(self.strict_equal(left, right))),
            BinaryOp::StrictNotEqual => Ok(Value::boolean(!self.strict_equal(left, right))),
            BinaryOp::Equal | BinaryOp::NotEqual => {
                let equal = self.abstract_equal(left, right)?;
                Ok(Value::boolean(if op == BinaryOp::Equal {
                    equal
                } else {
                    !equal
                }))
            }
            BinaryOp::LessThan
            | BinaryOp::LessThanOrEqual
            | BinaryOp::GreaterThan
            | BinaryOp::GreaterThanOrEqual => {
                let ordering = self.relational_compare(left, right)?;
                let result = match (op, ordering) {
                    (_, None) => false,
                    (BinaryOp::LessThan, Some(order)) => order == Ordering::Less,
                    (BinaryOp::LessThanOrEqual, Some(order)) => order != Ordering::Greater,
                    (BinaryOp::GreaterThan, Some(order)) => order == Ordering::Greater,
                    (BinaryOp::GreaterThanOrEqual, Some(order)) => order != Ordering::Less,
                    _ => unreachable!(),
                };
                Ok(Value::boolean(result))
            }
            BinaryOp::InstanceOf => self.instance_of(left, right).map(Value::boolean),
            BinaryOp::In => {
                let key = self.value_to_string(left, 0)?;
                self.has_property(right, &key).map(Value::boolean)
            }
            BinaryOp::Add => self.add(left, right),
            BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder
            | BinaryOp::Exponent
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::ShiftLeft
            | BinaryOp::ShiftRight
            | BinaryOp::UnsignedShiftRight => self.numeric_binary(op, left, right),
        }
    }

    fn add(&mut self, left: Value, right: Value) -> Result<Value, EvalFailure> {
        let left_string = self.add_string_primitive(left)?;
        let right_string = self.add_string_primitive(right)?;
        if left_string.is_some() || right_string.is_some() {
            let mut text = match left_string {
                Some(text) => text,
                None => self.value_to_string(left, 0)?,
            };
            match right_string {
                Some(right) => text.push_str(&right),
                None => text.push_str(&self.value_to_string(right, 0)?),
            }
            return self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime);
        }
        let left_bigint = self.bigint_text(left).map(str::to_owned);
        let right_bigint = self.bigint_text(right).map(str::to_owned);
        match (left_bigint, right_bigint) {
            (Some(left), Some(right)) => {
                let sum = bigint_i128(&left)?
                    .checked_add(bigint_i128(&right)?)
                    .ok_or(EvalFailure::Throw(ThrowOrigin::RangeError {
                        operation: "bigint add overflow",
                    }))?;
                return self
                    .allocate(HeapEntry::BigInt(sum.to_string()))
                    .map_err(EvalFailure::Runtime);
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "add bigint and number",
                }));
            }
            (None, None) => {}
        }
        let left = numeric_f64(self.to_number(left)?).unwrap();
        let right = numeric_f64(self.to_number(right)?).unwrap();
        Ok(number_value(left + right))
    }

    fn numeric_binary(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
    ) -> Result<Value, EvalFailure> {
        let left_bigint = self.bigint_text(left).map(str::to_owned);
        let right_bigint = self.bigint_text(right).map(str::to_owned);
        if left_bigint.is_some() || right_bigint.is_some() {
            let (Some(left), Some(right)) = (left_bigint, right_bigint) else {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "mix bigint and number",
                }));
            };
            let result = bigint_binary(op, &left, &right)?;
            return self
                .allocate(HeapEntry::BigInt(result))
                .map_err(EvalFailure::Runtime);
        }
        let left = numeric_f64(self.to_number(left)?).unwrap();
        let right = numeric_f64(self.to_number(right)?).unwrap();
        let value = match op {
            BinaryOp::Subtract => number_value(left - right),
            BinaryOp::Multiply => number_value(left * right),
            BinaryOp::Divide => Value::number(left / right),
            BinaryOp::Remainder => Value::number(left % right),
            BinaryOp::Exponent => Value::number(left.powf(right)),
            BinaryOp::BitAnd => Value::int32((to_int32(left) & to_int32(right)) as u32),
            BinaryOp::BitOr => Value::int32((to_int32(left) | to_int32(right)) as u32),
            BinaryOp::BitXor => Value::int32((to_int32(left) ^ to_int32(right)) as u32),
            BinaryOp::ShiftLeft => {
                Value::int32(to_int32(left).wrapping_shl(to_uint32(right) & 31) as u32)
            }
            BinaryOp::ShiftRight => {
                Value::int32((to_int32(left) >> (to_uint32(right) & 31)) as u32)
            }
            BinaryOp::UnsignedShiftRight => {
                number_value((to_uint32(left) >> (to_uint32(right) & 31)) as f64)
            }
            _ => unreachable!("numeric binary operator partition"),
        };
        Ok(value)
    }

    fn to_number(&self, value: Value) -> Result<Value, EvalFailure> {
        match value.decode() {
            Some(Decoded::Number(_)) | Some(Decoded::Int32(_)) => Ok(value),
            Some(Decoded::Undefined) => Ok(Value::number(f64::NAN)),
            Some(Decoded::Null) => Ok(Value::int32(0)),
            Some(Decoded::Boolean(value)) => Ok(Value::int32(u32::from(value))),
            Some(Decoded::Hole) | Some(Decoded::Uninitialized) => Ok(Value::number(f64::NAN)),
            Some(Decoded::HeapRef(_)) => match self
                .runtime_slot(value)
                .map_err(EvalFailure::Runtime)?
            {
                Some(index) => match &self.heap[index] {
                    HeapEntry::String(text) => Ok(number_value(parse_number(text))),
                    HeapEntry::BigInt(_) => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "convert bigint to number",
                    })),
                    HeapEntry::Array { elements, .. } if elements.is_empty() => Ok(Value::int32(0)),
                    HeapEntry::Array { elements, .. } if elements.len() == 1 => {
                        self.to_number(elements[0])
                    }
                    HeapEntry::Object { .. }
                    | HeapEntry::Array { .. }
                    | HeapEntry::Function { .. } => Ok(Value::number(f64::NAN)),
                },
                None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "coerce host object to number",
                })),
            },
            None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                value,
            })),
        }
    }

    fn truthy(&self, value: Value) -> bool {
        match value.decode() {
            Some(Decoded::Number(number)) => number != 0.0 && !number.is_nan(),
            Some(Decoded::Int32(value)) => value != 0,
            Some(Decoded::Undefined | Decoded::Null | Decoded::Hole | Decoded::Uninitialized)
            | None => false,
            Some(Decoded::Boolean(value)) => value,
            Some(Decoded::HeapRef(_)) => match self.runtime_slot(value) {
                Ok(Some(index)) => match &self.heap[index] {
                    HeapEntry::String(text) | HeapEntry::BigInt(text) => {
                        !text.is_empty() && text != "0"
                    }
                    HeapEntry::Object { .. }
                    | HeapEntry::Array { .. }
                    | HeapEntry::Function { .. } => true,
                },
                Ok(None) => true,
                Err(_) => false,
            },
        }
    }

    fn type_of(&self, value: Value) -> &'static str {
        match value.decode() {
            Some(Decoded::Undefined | Decoded::Hole | Decoded::Uninitialized) | None => "undefined",
            Some(Decoded::Number(_) | Decoded::Int32(_)) => "number",
            Some(Decoded::Null) => "object",
            Some(Decoded::Boolean(_)) => "boolean",
            Some(Decoded::HeapRef(_)) => match self.runtime_slot(value) {
                Ok(Some(index)) => match &self.heap[index] {
                    HeapEntry::String(_) => "string",
                    HeapEntry::BigInt(_) => "bigint",
                    HeapEntry::Function { .. } => "function",
                    HeapEntry::Object { .. } | HeapEntry::Array { .. } => "object",
                },
                _ => "object",
            },
        }
    }

    fn strict_equal(&self, left: Value, right: Value) -> bool {
        match (left.decode(), right.decode()) {
            (Some(Decoded::Number(a)), Some(Decoded::Number(b))) => a == b,
            (Some(Decoded::Number(a)), Some(Decoded::Int32(b)))
            | (Some(Decoded::Int32(b)), Some(Decoded::Number(a))) => a == f64::from(b as i32),
            (Some(Decoded::Int32(a)), Some(Decoded::Int32(b))) => a == b,
            (Some(Decoded::HeapRef(_)), Some(Decoded::HeapRef(_))) => {
                match (self.runtime_slot(left), self.runtime_slot(right)) {
                    (Ok(Some(a)), Ok(Some(b))) => match (&self.heap[a], &self.heap[b]) {
                        (HeapEntry::String(a), HeapEntry::String(b))
                        | (HeapEntry::BigInt(a), HeapEntry::BigInt(b)) => a == b,
                        _ => left == right,
                    },
                    _ => left == right,
                }
            }
            _ => left == right,
        }
    }

    fn abstract_equal(&self, left: Value, right: Value) -> Result<bool, EvalFailure> {
        if self.strict_equal(left, right) {
            return Ok(true);
        }
        if matches!(
            (left.decode(), right.decode()),
            (Some(Decoded::Null), Some(Decoded::Undefined))
                | (Some(Decoded::Undefined), Some(Decoded::Null))
        ) {
            return Ok(true);
        }
        let left_number = self.to_number(left);
        let right_number = self.to_number(right);
        match (left_number, right_number) {
            (Ok(left), Ok(right)) => Ok(numeric_f64(left).unwrap() == numeric_f64(right).unwrap()),
            _ => Ok(false),
        }
    }

    fn relational_compare(
        &self,
        left: Value,
        right: Value,
    ) -> Result<Option<Ordering>, EvalFailure> {
        if let (Some(left), Some(right)) = (self.string_text(left), self.string_text(right)) {
            return Ok(Some(left.cmp(right)));
        }
        if let (Some(left), Some(right)) = (self.bigint_text(left), self.bigint_text(right)) {
            return Ok(Some(bigint_i128(left)?.cmp(&bigint_i128(right)?)));
        }
        let left = numeric_f64(self.to_number(left)?).unwrap();
        let right = numeric_f64(self.to_number(right)?).unwrap();
        Ok(left.partial_cmp(&right))
    }

    fn instance_of(&mut self, value: Value, constructor: Value) -> Result<bool, EvalFailure> {
        match self
            .runtime_slot(constructor)
            .map_err(EvalFailure::Runtime)?
        {
            Some(index) => {
                if !matches!(self.heap[index], HeapEntry::Function { .. }) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "instanceof",
                    }));
                }
                match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
                    Some(value_index) => Ok(match &self.heap[value_index] {
                        HeapEntry::Object {
                            constructor: actual,
                            ..
                        }
                        | HeapEntry::Array {
                            constructor: actual,
                            ..
                        } => *actual == Some(constructor),
                        HeapEntry::Function { .. }
                        | HeapEntry::String(_)
                        | HeapEntry::BigInt(_) => false,
                    }),
                    None => Ok(false),
                }
            }
            None => match constructor.decode() {
                Some(Decoded::HeapRef(_)) => self
                    .host
                    .instance_of(value, constructor)
                    .map_err(|_| EvalFailure::Throw(ThrowOrigin::Host)),
                _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "instanceof",
                })),
            },
        }
    }

    fn add_string_primitive(&self, value: Value) -> Result<Option<String>, EvalFailure> {
        match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::String(text) => Ok(Some(text.clone())),
                HeapEntry::Object { .. } | HeapEntry::Array { .. } | HeapEntry::Function { .. } => {
                    self.value_to_string(value, 0).map(Some)
                }
                HeapEntry::BigInt(_) => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn value_to_string(&self, value: Value, depth: u8) -> Result<String, EvalFailure> {
        if depth >= 32 {
            return Ok(String::new());
        }
        match value.decode() {
            Some(Decoded::Number(number)) => Ok(format_number(number)),
            Some(Decoded::Int32(raw)) => Ok((raw as i32).to_string()),
            Some(Decoded::Undefined | Decoded::Uninitialized) => Ok("undefined".to_owned()),
            Some(Decoded::Null) => Ok("null".to_owned()),
            Some(Decoded::Boolean(value)) => Ok(value.to_string()),
            Some(Decoded::Hole) => Ok(String::new()),
            Some(Decoded::HeapRef(_)) => {
                match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
                    Some(index) => match &self.heap[index] {
                        HeapEntry::String(text) | HeapEntry::BigInt(text) => Ok(text.clone()),
                        HeapEntry::Object { .. } => Ok("[object Object]".to_owned()),
                        HeapEntry::Function { function, .. } => {
                            let flags = self.module.functions()[function.get() as usize].flags();
                            Ok(match (flags.is_async, flags.is_generator) {
                                (true, true) => "async function* () { [bytecode] }",
                                (true, false) => "async function () { [bytecode] }",
                                (false, true) => "function* () { [bytecode] }",
                                (false, false) => "function () { [bytecode] }",
                            }
                            .to_owned())
                        }
                        HeapEntry::Array { elements, .. } => {
                            let mut text = String::new();
                            for (index, element) in elements.iter().copied().enumerate() {
                                if index != 0 {
                                    text.push(',');
                                }
                                if element != Value::HOLE
                                    && element != Value::NULL
                                    && element != Value::UNDEFINED
                                {
                                    text.push_str(&self.value_to_string(element, depth + 1)?);
                                }
                            }
                            Ok(text)
                        }
                    },
                    None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "coerce host object to string",
                    })),
                }
            }
            None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidHostValue {
                value,
            })),
        }
    }

    fn string_text(&self, value: Value) -> Option<&str> {
        let index = self.runtime_slot(value).ok()??;
        match &self.heap[index] {
            HeapEntry::String(text) => Some(text),
            _ => None,
        }
    }

    fn bigint_text(&self, value: Value) -> Option<&str> {
        let index = self.runtime_slot(value).ok()??;
        match &self.heap[index] {
            HeapEntry::BigInt(text) => Some(text),
            _ => None,
        }
    }

    fn is_object(&self, value: Value) -> bool {
        match self.runtime_slot(value) {
            Ok(Some(index)) => matches!(
                self.heap[index],
                HeapEntry::Object { .. } | HeapEntry::Array { .. } | HeapEntry::Function { .. }
            ),
            Ok(None) => matches!(value.decode(), Some(Decoded::HeapRef(_))),
            Err(_) => false,
        }
    }
}

fn innermost_handler(function: &Function, pc: usize) -> Option<bamts_bytecode::ExceptionHandler> {
    function
        .handlers()
        .iter()
        .copied()
        .filter(|handler| handler.start.get() as usize <= pc && pc < handler.end.get() as usize)
        .max_by(|left, right| {
            left.start
                .get()
                .cmp(&right.start.get())
                .then_with(|| right.end.get().cmp(&left.end.get()))
        })
}

fn numeric_f64(value: Value) -> Option<f64> {
    match value.decode()? {
        Decoded::Number(number) => Some(number),
        Decoded::Int32(raw) => Some(f64::from(raw as i32)),
        _ => None,
    }
}

fn number_value(number: f64) -> Value {
    if number.is_finite()
        && number.fract() == 0.0
        && number >= f64::from(i32::MIN)
        && number <= f64::from(i32::MAX)
    {
        Value::int32(number as i32 as u32)
    } else {
        Value::number(number)
    }
}

fn parse_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    }
}

fn format_number(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number == f64::INFINITY {
        "Infinity".to_owned()
    } else if number == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else if number == 0.0 {
        "0".to_owned()
    } else {
        number.to_string()
    }
}

fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() || number == 0.0 {
        0
    } else {
        number.trunc().rem_euclid(4_294_967_296.0) as u32
    }
}

fn to_int32(number: f64) -> i32 {
    to_uint32(number) as i32
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX).then_some(index)
}

fn exact_array_length(value: Value) -> Option<usize> {
    let number = numeric_f64(value)?;
    if number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number <= u32::MAX as f64 {
        Some(number as usize)
    } else {
        None
    }
}

fn bigint_i128(text: &str) -> Result<i128, EvalFailure> {
    text.parse::<i128>().map_err(|_| {
        EvalFailure::Throw(ThrowOrigin::RangeError {
            operation: "bigint magnitude exceeds runtime width",
        })
    })
}

fn bigint_binary(op: BinaryOp, left: &str, right: &str) -> Result<String, EvalFailure> {
    let left = bigint_i128(left)?;
    let right = bigint_i128(right)?;
    let overflow =
        |operation: &'static str| EvalFailure::Throw(ThrowOrigin::RangeError { operation });
    let result = match op {
        BinaryOp::Subtract => left
            .checked_sub(right)
            .ok_or_else(|| overflow("bigint subtract overflow"))?,
        BinaryOp::Multiply => left
            .checked_mul(right)
            .ok_or_else(|| overflow("bigint multiply overflow"))?,
        BinaryOp::Divide => {
            if right == 0 {
                return Err(EvalFailure::Throw(ThrowOrigin::RangeError {
                    operation: "bigint division by zero",
                }));
            }
            left.checked_div(right)
                .ok_or_else(|| overflow("bigint divide overflow"))?
        }
        BinaryOp::Remainder => {
            if right == 0 {
                return Err(EvalFailure::Throw(ThrowOrigin::RangeError {
                    operation: "bigint remainder by zero",
                }));
            }
            left.checked_rem(right)
                .ok_or_else(|| overflow("bigint remainder overflow"))?
        }
        BinaryOp::Exponent => {
            if right < 0 {
                return Err(EvalFailure::Throw(ThrowOrigin::RangeError {
                    operation: "bigint negative exponent",
                }));
            }
            let exponent =
                u32::try_from(right).map_err(|_| overflow("bigint exponent overflow"))?;
            left.checked_pow(exponent)
                .ok_or_else(|| overflow("bigint exponent overflow"))?
        }
        BinaryOp::BitAnd => left & right,
        BinaryOp::BitOr => left | right,
        BinaryOp::BitXor => left ^ right,
        BinaryOp::ShiftLeft | BinaryOp::ShiftRight => {
            let left_shift = (op == BinaryOp::ShiftLeft) == (right >= 0);
            let amount =
                u32::try_from(right.unsigned_abs()).map_err(|_| overflow("bigint shift width"))?;
            let shifted = if left_shift {
                left.checked_shl(amount)
            } else {
                left.checked_shr(amount)
            };
            shifted.ok_or_else(|| overflow("bigint shift overflow"))?
        }
        BinaryOp::UnsignedShiftRight => {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "unsigned shift on bigint",
            }));
        }
        _ => unreachable!("bigint arithmetic partition"),
    };
    Ok(result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_bytecode::{ExceptionHandler, FunctionFlags, NumberBits, Register};

    fn reg(raw: u32) -> Register {
        Register::new(raw)
    }
    fn pc(raw: u32) -> Pc {
        Pc::new(raw)
    }
    fn cid(raw: u32) -> ConstantId {
        ConstantId::new(raw)
    }

    fn function(
        parameters: u32,
        registers: u32,
        code: Vec<Instruction>,
        handlers: Vec<ExceptionHandler>,
    ) -> Function {
        Function::new(
            None,
            parameters,
            registers,
            FunctionFlags::default(),
            code,
            handlers,
        )
    }

    fn verified(constants: Vec<Constant>, functions: Vec<Function>) -> Module<Verified> {
        Module::new(constants, functions, FunctionId::new(0))
            .verify()
            .expect("valid test bytecode")
    }

    #[derive(Default)]
    struct TestHost {
        awaited: Option<Result<Value, HostThrow>>,
        imported: Option<Result<Value, HostThrow>>,
    }
    impl Host for TestHost {
        fn awaited(&mut self, _value: Value) -> Result<Value, HostThrow> {
            self.awaited.take().expect("scripted await")
        }
        fn import(&mut self, _specifier: &str) -> Result<Value, HostThrow> {
            self.imported.take().expect("scripted import")
        }
    }

    #[test]
    fn object_and_function_values_have_stable_distinct_heap_identity() {
        let module = verified(
            vec![],
            vec![function(
                0,
                5,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::CreateObject { dst: reg(1) },
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::StrictEqual,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::Move {
                        dst: reg(3),
                        src: reg(0),
                    },
                    Instruction::Binary {
                        dst: reg(4),
                        op: BinaryOp::StrictEqual,
                        left: reg(0),
                        right: reg(3),
                    },
                    Instruction::Return { value: reg(4) },
                ],
                vec![],
            )],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.entry_registers[2], Value::FALSE);
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn property_set_get_delete_and_array_holes_are_real_mutations() {
        let module = verified(
            vec![Constant::String("0".into())],
            vec![function(
                0,
                5,
                vec![
                    Instruction::CreateArray { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: cid(0),
                        value: reg(1),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: cid(0),
                    },
                    Instruction::DeleteProperty {
                        dst: reg(3),
                        object: reg(0),
                        key: cid(0),
                    },
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(0),
                        key: cid(0),
                    },
                    Instruction::Return { value: reg(3) },
                ],
                vec![],
            )],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.entry_registers[1], execution.entry_registers[2]);
        assert_eq!(execution.entry_registers[4], Value::UNDEFINED);
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn internal_call_copies_argument_window_into_parameter_registers() {
        let entry = function(
            0,
            4,
            vec![
                Instruction::DefineFunction {
                    dst: reg(0),
                    function: FunctionId::new(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(1),
                },
                Instruction::Call {
                    dst: reg(3),
                    callee: reg(0),
                    this_value: reg(1),
                    args_start: reg(2),
                    arg_count: 1,
                },
                Instruction::Return { value: reg(3) },
            ],
            vec![],
        );
        let callee = function(1, 1, vec![Instruction::Return { value: reg(0) }], vec![]);
        let module = verified(
            vec![Constant::Undefined, Constant::Int32(42)],
            vec![entry, callee],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::int32(42));
    }

    #[test]
    fn catch_register_receives_exact_thrown_value() {
        let module = verified(
            vec![Constant::Int32(9)],
            vec![function(
                0,
                2,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::Throw { value: reg(0) },
                    Instruction::Return { value: reg(1) },
                ],
                vec![ExceptionHandler {
                    start: pc(1),
                    end: pc(2),
                    handler: pc(2),
                    catch_register: reg(1),
                }],
            )],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::int32(9));
    }

    #[test]
    fn callee_throw_unwinds_to_call_site_handler() {
        let entry = function(
            0,
            4,
            vec![
                Instruction::DefineFunction {
                    dst: reg(0),
                    function: FunctionId::new(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::Call {
                    dst: reg(2),
                    callee: reg(0),
                    this_value: reg(1),
                    args_start: reg(1),
                    arg_count: 0,
                },
                Instruction::Halt,
                Instruction::Return { value: reg(3) },
            ],
            vec![ExceptionHandler {
                start: pc(2),
                end: pc(3),
                handler: pc(4),
                catch_register: reg(3),
            }],
        );
        let callee = function(
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(1),
                },
                Instruction::Throw { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![Constant::Undefined, Constant::Int32(7)],
            vec![entry, callee],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::int32(7));
    }

    #[test]
    fn host_import_failure_is_catchable_at_import_pc() {
        let module = verified(
            vec![Constant::String("x".into())],
            vec![function(
                0,
                2,
                vec![
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(0),
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
        );
        let thrown = Value::int32(17);
        let mut host = TestHost {
            imported: Some(Err(HostThrow { value: thrown })),
            ..TestHost::default()
        };
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, thrown);
    }

    #[test]
    fn malformed_host_value_is_typed_error_with_source_context() {
        let module = verified(
            vec![Constant::String("main".into()), Constant::Undefined],
            vec![Function::new(
                Some(cid(0)),
                0,
                1,
                FunctionFlags::default(),
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(1),
                    },
                    Instruction::Suspend {
                        dst: reg(0),
                        src: reg(0),
                        resume: pc(2),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                vec![],
            )],
        );
        let malformed = Value::from_bits(Value::CANON_NAN | (5_u64 << 48) | 7);
        let mut host = TestHost {
            awaited: Some(Ok(malformed)),
            ..TestHost::default()
        };
        let error = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert_eq!(error.function, FunctionId::new(0));
        assert_eq!(error.pc, pc(1));
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::InvalidHostValue { .. }
        ));
        assert!(matches!(
            error.source.instruction,
            Instruction::Suspend { .. }
        ));
        assert_eq!(error.source.function_name.as_deref(), Some("main"));
    }

    #[test]
    fn heap_and_register_limits_fail_before_unbounded_growth() {
        let module = verified(
            vec![],
            vec![function(
                0,
                2,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::CreateObject { dst: reg(1) },
                    Instruction::Halt,
                ],
                vec![],
            )],
        );
        let mut host = TestHost::default();
        let error = Machine::new(
            &module,
            &mut host,
            Limits {
                max_heap_slots: 1,
                ..Limits::default()
            },
        )
        .run()
        .unwrap_err();
        assert_eq!(error.pc, pc(1));
        assert_eq!(
            error.kind,
            RuntimeErrorKind::HeapSlotLimitExceeded { limit: 1 }
        );

        let mut host = TestHost::default();
        let error = Machine::new(
            &module,
            &mut host,
            Limits {
                max_total_registers: 1,
                ..Limits::default()
            },
        )
        .run()
        .unwrap_err();
        assert_eq!(
            error.kind,
            RuntimeErrorKind::RegisterLimitExceeded { limit: 1 }
        );
    }

    #[test]
    fn u32_registers_and_instruction_pcs_do_not_truncate_at_127() {
        let mut code = vec![Instruction::LoadConst {
            dst: reg(0),
            constant: cid(0),
        }];
        for register in 1..=199 {
            code.push(Instruction::Move {
                dst: reg(register),
                src: reg(register - 1),
            });
        }
        code.push(Instruction::Return { value: reg(199) });
        let module = verified(
            vec![Constant::Number(NumberBits::from_f64(3.5))],
            vec![function(0, 200, code, vec![])],
        );
        let mut host = TestHost::default();
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::number(3.5));
        assert_eq!(execution.entry_registers[199], Value::number(3.5));
    }

    #[test]
    fn call_depth_and_argument_limits_are_deterministic() {
        let entry = function(
            0,
            3,
            vec![
                Instruction::DefineFunction {
                    dst: reg(0),
                    function: FunctionId::new(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::Call {
                    dst: reg(2),
                    callee: reg(0),
                    this_value: reg(1),
                    args_start: reg(1),
                    arg_count: 1,
                },
                Instruction::Halt,
            ],
            vec![],
        );
        let callee = function(
            1,
            3,
            vec![
                Instruction::DefineFunction {
                    dst: reg(0),
                    function: FunctionId::new(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::Call {
                    dst: reg(2),
                    callee: reg(0),
                    this_value: reg(1),
                    args_start: reg(1),
                    arg_count: 1,
                },
                Instruction::Return { value: reg(2) },
            ],
            vec![],
        );
        let module = verified(vec![Constant::Undefined], vec![entry, callee]);
        let mut host = TestHost::default();
        let error = Machine::new(
            &module,
            &mut host,
            Limits {
                max_call_depth: 2,
                ..Limits::default()
            },
        )
        .run()
        .unwrap_err();
        assert_eq!(error.kind, RuntimeErrorKind::CallDepthExceeded { limit: 2 });

        let mut host = TestHost::default();
        let error = Machine::new(
            &module,
            &mut host,
            Limits {
                max_argument_count: 0,
                ..Limits::default()
            },
        )
        .run()
        .unwrap_err();
        assert_eq!(
            error.kind,
            RuntimeErrorKind::ArgumentLimitExceeded {
                limit: 0,
                requested: 1
            }
        );
    }
}
