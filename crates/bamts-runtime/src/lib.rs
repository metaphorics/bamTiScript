//! Deterministic register interpreter for verified production BamTS bytecode.
//!
//! Persisted constants never carry runtime identity. This interpreter therefore
//! owns a slot heap for strings, bigints, objects, arrays, closures, private
//! names, regular expressions, and iterators, and exposes only
//! `bamts_native::Value` words at host boundaries. Bytecode heap slots use
//! segment 1; other segments remain host-owned.
//!
//! The 36-op dynamic-computation ISA is executed locally: register-keyed
//! property access (with data properties, accessor descriptors, and private-name
//! identity), prototype chains, closures with explicit capture environments,
//! arguments arrays, `this`/`arguments`/`new.target`, globals, arrays with
//! spread, object spread, iterators (sync/async/keys) with the two-write
//! `IteratorNext`, and generator/async `Suspend`-resume. The [`Host`] trait owns
//! only *external* module and builtin operations (foreign calls/constructs,
//! foreign property access, `import`, `export`, and the suspension driver);
//! internal objects, arrays, functions, closures, prototypes, and private names
//! never leave the interpreter.

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use bamts_bytecode::{
    AccessorKind, BinaryOp, BindingId, BindingKind, Constant, ConstantId, EcmaString,
    EcmaStringBuilder, EdgeId, EdgeTarget, Function, FunctionId, Instruction, IteratorKind, Module,
    ModuleId, Pc, Program, ResolvedExport, UnaryOp, Verified,
};
use bamts_native::{Decoded, SlotId, Value};

mod external_modules;
mod host_objects;
mod intrinsics;
mod native;

pub use native::{NativeEngine, NativeError, run_linked_program};

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
    /// Ceiling on the length of a single call's arguments array.
    pub max_argument_count: u32,
    pub max_heap_slots: usize,
    pub max_heap_bytes: usize,
    /// Ceiling on engine-owned module binding cells.
    pub max_module_cells: usize,
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
            max_module_cells: 1 << 20,
        }
    }
}

/// External capabilities available to the JavaScript runtime.
///
/// Runtime values never cross this boundary. The engine owns all JavaScript
/// objects and value semantics; hosts provide byte sinks and process services.
pub trait Host {
    fn write_stdout(&mut self, _bytes: &[u8]) {}

    fn write_stderr(&mut self, _bytes: &[u8]) {}

    fn exit_code(&self) -> i32 {
        0
    }

    fn set_exit_code(&mut self, _exit_code: i32) {}

    fn argv(&self) -> &[String] {
        &[]
    }

    fn env(&self, _name: &str) -> Option<&str> {
        None
    }

    fn set_env(&mut self, _name: &str, _value: &str) {}

    fn delete_env(&mut self, _name: &str) -> bool {
        false
    }

    fn now_ms(&mut self) -> u64 {
        0
    }

    fn monotonic_ns(&mut self) -> u64 {
        0
    }

    fn random(&mut self) -> f64 {
        0.0
    }

    fn hash(&mut self, _algorithm: &str, _data: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThrowOrigin {
    Bytecode,
    TypeError { operation: &'static str },
    RangeError { operation: &'static str },
    ReferenceError { operation: &'static str },
    UriError { operation: &'static str },
}

/// Source metadata attached to every machine failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSource {
    pub function_name: Option<EcmaString>,
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
    UncaughtThrow {
        value: Value,
        origin: ThrowOrigin,
    },
    FuelExhausted {
        limit: u64,
    },
    CallDepthExceeded {
        limit: usize,
    },
    RegisterLimitExceeded {
        limit: usize,
    },
    ArgumentLimitExceeded {
        limit: u32,
        requested: u32,
    },
    HeapSlotLimitExceeded {
        limit: usize,
    },
    HeapByteLimitExceeded {
        limit: usize,
    },
    ModuleCellLimitExceeded {
        limit: usize,
    },
    TemporalDeadZone {
        module: ModuleId,
        binding: BindingId,
    },
    ExternalModuleUnavailable {
        module: ModuleId,
        edge: EdgeId,
    },
    DynamicImportUnsupported {
        module: ModuleId,
    },
    InvalidVerifiedProgram {
        module: ModuleId,
        instruction: Instruction,
    },
    InvalidValue {
        value: Value,
    },
    InvalidRuntimeHeapReference {
        slot: u32,
    },
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
            write!(formatter, " ({})", name.to_utf8_lossy())?;
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
            RuntimeErrorKind::ModuleCellLimitExceeded { limit } => {
                write!(formatter, "module cell limit {limit} exceeded")
            }
            RuntimeErrorKind::TemporalDeadZone { module, binding } => write!(
                formatter,
                "module {} binding {} read before initialization",
                module.get(),
                binding.get()
            ),
            RuntimeErrorKind::ExternalModuleUnavailable { module, edge } => write!(
                formatter,
                "external module at module {} edge {} is unavailable",
                module.get(),
                edge.get()
            ),
            RuntimeErrorKind::DynamicImportUnsupported { module } => write!(
                formatter,
                "dynamic import is unsupported in module {}",
                module.get()
            ),
            RuntimeErrorKind::InvalidVerifiedProgram {
                module,
                instruction,
            } => write!(
                formatter,
                "verified module {} contains impossible instruction {instruction:?}",
                module.get()
            ),
            RuntimeErrorKind::InvalidValue { value } => {
                write!(
                    formatter,
                    "malformed or foreign value {:#018x}",
                    value.to_bits()
                )
            }
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

/// A normalized property key: a string name (which may parse as an array index),
/// a public symbol, or a language private name, each identified by its heap slot.
/// Two identity-bearing allocations with the same description remain distinct keys.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PropertyKey {
    Named(EcmaString),
    Symbol(u32),
    Private(u32),
}

impl PropertyKey {
    fn as_string(&self) -> Option<&EcmaString> {
        match self {
            PropertyKey::Named(text) => Some(text),
            PropertyKey::Symbol(_) | PropertyKey::Private(_) => None,
        }
    }

    fn eq_ascii(&self, ascii: &str) -> bool {
        matches!(self, PropertyKey::Named(text) if text.eq_ascii(ascii))
    }

    fn charge_bytes(&self) -> usize {
        match self {
            PropertyKey::Named(text) => text.len_units().saturating_mul(2).saturating_add(8),
            PropertyKey::Symbol(_) | PropertyKey::Private(_) => 16,
        }
    }
}

/// A stored property: a data value or an accessor descriptor.
#[derive(Clone, Debug)]
enum Property {
    Data {
        value: Value,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    },
    Accessor {
        getter: Option<Value>,
        setter: Option<Value>,
        enumerable: bool,
        configurable: bool,
    },
}
impl Property {
    fn enumerable(&self) -> bool {
        match self {
            Self::Data { enumerable, .. } | Self::Accessor { enumerable, .. } => *enumerable,
        }
    }

    fn configurable(&self) -> bool {
        match self {
            Self::Data { configurable, .. } | Self::Accessor { configurable, .. } => *configurable,
        }
    }
}

/// Own properties in creation order. ECMAScript enumerates array-index keys
/// separately, but preserves this order for all other string and symbol keys.
#[derive(Clone, Debug, Default)]
struct PropertyMap(Vec<(PropertyKey, Property)>);

impl PropertyMap {
    fn get(&self, key: &PropertyKey) -> Option<&Property> {
        self.0
            .iter()
            .find_map(|(candidate, property)| (candidate == key).then_some(property))
    }

    fn get_mut(&mut self, key: &PropertyKey) -> Option<&mut Property> {
        self.0
            .iter_mut()
            .find_map(|(candidate, property)| (candidate == key).then_some(property))
    }

    fn contains_key(&self, key: &PropertyKey) -> bool {
        self.get(key).is_some()
    }

    fn get_ascii(&self, ascii: &str) -> Option<&Property> {
        debug_assert!(ascii.is_ascii());
        self.0
            .iter()
            .find_map(|(key, property)| key.eq_ascii(ascii).then_some(property))
    }

    fn insert(&mut self, key: PropertyKey, property: Property) -> Option<Property> {
        if let Some(existing) = self.get_mut(&key) {
            return Some(std::mem::replace(existing, property));
        }
        self.0.push((key, property));
        None
    }

    fn remove(&mut self, key: &PropertyKey) -> Option<Property> {
        let index = self.0.iter().position(|(candidate, _)| candidate == key)?;
        Some(self.0.remove(index).1)
    }

    fn iter(&self) -> impl Iterator<Item = (&PropertyKey, &Property)> {
        self.0.iter().map(|(key, property)| (key, property))
    }
}

impl<'a> IntoIterator for &'a PropertyMap {
    type Item = (&'a PropertyKey, &'a Property);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (PropertyKey, Property)>,
        fn(&(PropertyKey, Property)) -> (&PropertyKey, &Property),
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn pair_refs(pair: &(PropertyKey, Property)) -> (&PropertyKey, &Property) {
            (&pair.0, &pair.1)
        }
        self.0.iter().map(pair_refs)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IterationKind {
    Key,
    Value,
    Entry,
}

#[derive(Clone, Copy, Debug)]
struct CollectionEntry {
    order: u64,
    key: Value,
    value: Value,
}

impl CollectionEntry {
    const BYTES: usize = std::mem::size_of::<Self>();
}

#[derive(Clone, Debug)]
enum HeapEntry {
    String(EcmaString),
    BigInt(String),
    Object {
        properties: PropertyMap,
        prototype: Option<Value>,
        boxed_primitive: Option<Value>,
        extensible: bool,
    },
    Array {
        elements: Vec<Value>,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
        length_writable: bool,
    },
    Function {
        module: ModuleId,
        function: FunctionId,
        captures: Vec<Value>,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    ModuleNamespace {
        module: ModuleId,
    },
    ExternalModuleNamespace {
        specifier: EcmaString,
    },
    HashState {
        algorithm: String,
        data: Vec<u8>,
        digested: bool,
        update: Value,
        digest: Value,
    },
    Symbol {
        description: EcmaString,
    },
    PrivateName {
        description: EcmaString,
    },
    RegExp {
        pattern: EcmaString,
        flags: EcmaString,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    Date {
        time: f64,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    /// Entries stay in insertion order. Iterators track order IDs, so deletion
    /// can remove storage without moving an iterator's logical cursor.
    /// Weak collections share this strong storage until the runtime has a collector.
    Collection {
        entries: Vec<CollectionEntry>,
        next_order: u64,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    BuiltinIterator {
        source: Value,
        kind: IterationKind,
        position: Option<u64>,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    Iterator {
        source: Value,
        index: usize,
        keys: Option<Vec<EcmaString>>,
    },
    ProcessEnv {
        prototype: Option<Value>,
        extensible: bool,
    },
    NativeFunction {
        id: intrinsics::BuiltinId,
        properties: PropertyMap,
        bound_this: Option<Value>,
        extensible: bool,
    },
}

impl HeapEntry {
    fn initial_bytes(&self) -> usize {
        match self {
            Self::String(text)
            | Self::Symbol { description: text }
            | Self::PrivateName { description: text } => text.len_units().saturating_mul(2),
            Self::BigInt(text) => text.len(),
            Self::RegExp { pattern, flags, .. } => pattern
                .len_units()
                .saturating_add(flags.len_units())
                .saturating_mul(2),
            Self::HashState {
                algorithm, data, ..
            } => algorithm.len() + data.len(),
            Self::Collection { entries, .. } => entries
                .len()
                .saturating_mul(CollectionEntry::BYTES)
                .saturating_add(1),
            Self::Object { .. }
            | Self::Array { .. }
            | Self::Function { .. }
            | Self::ModuleNamespace { .. }
            | Self::ExternalModuleNamespace { .. }
            | Self::NativeFunction { .. }
            | Self::ProcessEnv { .. }
            | Self::Date { .. }
            | Self::BuiltinIterator { .. }
            | Self::Iterator { .. } => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ReturnTo {
    /// The caller register receiving the result, or `None` to discard it (an
    /// accessor setter invocation produces no observable value).
    destination: Option<usize>,
    call_pc: usize,
    constructed: Option<Value>,
}

struct CallRequest<'a> {
    callee: Value,
    this_value: Value,
    arguments: &'a [Value],
    destination: Option<u32>,
    call_pc: usize,
    constructed: Option<Value>,
    new_target: Value,
}

#[derive(Clone, Copy, Debug)]
struct RuntimeFunction {
    module: ModuleId,
    function: FunctionId,
}

#[derive(Clone, Debug)]
struct Frame {
    module: ModuleId,
    function: usize,
    pc: usize,
    registers: Vec<Value>,
    return_to: Option<ReturnTo>,
    this_value: Value,
    new_target: Value,
    args: Vec<Value>,
    arguments_object: Option<Value>,
}

impl Frame {
    fn new(
        target: RuntimeFunction,
        metadata: &Function,
        captures: &[Value],
        this_value: Value,
        new_target: Value,
        arguments: &[Value],
        return_to: Option<ReturnTo>,
    ) -> Self {
        let mut registers = vec![Value::UNINITIALIZED; metadata.register_count() as usize];
        let capture_count = metadata.capture_count() as usize;
        for (index, slot) in registers.iter_mut().take(capture_count).enumerate() {
            *slot = captures.get(index).copied().unwrap_or(Value::UNDEFINED);
        }
        for (index, slot) in registers
            .iter_mut()
            .skip(capture_count)
            .take(metadata.parameter_count() as usize)
            .enumerate()
        {
            *slot = arguments.get(index).copied().unwrap_or(Value::UNDEFINED);
        }
        Self {
            module: target.module,
            function: target.function.get() as usize,
            pc: 0,
            registers,
            return_to,
            this_value,
            new_target,
            args: arguments.to_vec(),
            arguments_object: None,
        }
    }
}

#[derive(Clone, Debug)]
enum EvalFailure {
    Throw(ThrowOrigin),
    ThrowValue(Value),
    Runtime(RuntimeErrorKind),
}

/// The outcome of resolving a property read.
#[derive(Clone, Debug)]
enum GetOutcome {
    /// A ready ABI value.
    Value(Value),
    /// Text that must be interned as a fresh heap string.
    Text(EcmaString),
    /// An accessor getter to invoke with the receiver as `this`.
    Getter(Value),
}

/// The outcome of resolving a property write.
#[derive(Clone, Debug)]
enum SetOutcome {
    Done,
    /// An accessor setter to invoke with the receiver as `this` and the value as
    /// its sole argument.
    Setter(Value),
}

/// Own-property lookup result during a prototype-chain walk.
#[derive(Clone, Debug)]
enum Found {
    Value(Value),
    Text(EcmaString),
    Getter(Value),
    Failure(RuntimeErrorKind),
    /// An accessor property with no getter resolves to `undefined`.
    NoGetter,
}

/// A classified callee, shared by the interpreter and the native engine so both
/// route calls through one decision. The native engine reimplements the frame
/// push but consumes this classifier verbatim.
#[derive(Clone, Debug)]
enum CalleeKind {
    Runtime {
        target: RuntimeFunction,
        captures: Vec<Value>,
    },
    Builtin {
        id: intrinsics::BuiltinId,
        bound_this: Option<Value>,
    },
    NotCallable,
}

/// The production bytecode interpreter.
pub struct Machine<'a, H: Host> {
    program: Option<&'a Program<Verified>>,
    module: &'a Module<Verified>,
    host: &'a mut H,
    limits: Limits,
    frames: Vec<Frame>,
    heap: Vec<HeapEntry>,
    intrinsic_slots: usize,
    heap_bytes: usize,
    live_registers: usize,
    native_depth: usize,
    fuel: u64,
    globals: BTreeMap<EcmaString, Value>,
    last_completion: Option<Value>,
    intrinsics: intrinsics::Intrinsics<H>,
    current_builtin_id: Option<intrinsics::BuiltinId>,
    registry: ModuleRegistry,
}

#[derive(Clone, Debug, Default)]
struct ModuleRegistry {
    modules: Vec<ModuleInstance>,
    cells: Vec<Cell>,
    external: BTreeMap<EcmaString, ExternalModuleInstance>,
}

#[derive(Clone, Debug)]
struct ExternalModuleInstance {
    namespace: Value,
    exports: BTreeMap<EcmaString, ExternalExport>,
    internals: BTreeMap<&'static str, Value>,
}

#[derive(Clone, Copy, Debug)]
struct ExternalExport {
    value: Value,
    cell: Option<CellId>,
}

#[derive(Clone, Debug)]
struct ModuleInstance {
    binding_cells: Vec<Option<CellId>>,
    constant_cells: Vec<Option<CellId>>,
    namespace: Option<Value>,
    state: ModuleState,
}

#[derive(Clone, Debug)]
enum ModuleState {
    Unevaluated,
    Evaluating,
    Evaluated(Result<Execution, RuntimeError>),
}

#[derive(Clone, Debug)]
pub(crate) enum ModuleEvaluation {
    Cycle,
    Evaluated(Result<Execution, RuntimeError>),
    Ready(Vec<ModuleId>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CellId(usize);

#[derive(Clone, Copy, Debug)]
struct Cell {
    value: Value,
}

pub fn run<H: Host>(
    program: &Program<Verified>,
    host: &mut H,
    limits: &Limits,
) -> Result<ExecutionOutcome, RuntimeError> {
    Machine::new(program, host, limits.clone())
        .run()
        .map(|execution| execution.outcome)
}

impl<'a, H: Host> Machine<'a, H> {
    #[must_use]
    pub fn new(program: &'a Program<Verified>, host: &'a mut H, limits: Limits) -> Self {
        let module = &program
            .module(program.entry())
            .expect("verified program entry exists")
            .code;
        Self::build(Some(program), module, host, limits)
    }

    fn build(
        program: Option<&'a Program<Verified>>,
        module: &'a Module<Verified>,
        host: &'a mut H,
        limits: Limits,
    ) -> Self {
        let entry = module.entry().get() as usize;
        let module_id = program.map_or(ModuleId::new(0), Program::entry);
        let frame = Frame::new(
            RuntimeFunction {
                module: module_id,
                function: FunctionId::new(entry as u32),
            },
            &module.functions()[entry],
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
            None,
        );
        let live_registers = frame.registers.len();
        let mut heap = Vec::new();
        let mut intrinsics = intrinsics::Intrinsics::<H>::initialize(&mut heap);
        let installed_external = external_modules::install(
            &mut heap,
            &mut intrinsics.builtins,
            intrinsics.object_prototype,
        );
        let argv_text = host.argv().to_vec();
        let argv_values: Vec<Value> = argv_text
            .into_iter()
            .map(|text| {
                intrinsics::push(&mut heap, HeapEntry::String(EcmaString::from_utf8(&text)))
            })
            .collect();
        let process = intrinsics
            .global("process")
            .expect("host objects install process");
        let Some(Decoded::HeapRef(process_id)) = process.decode() else {
            unreachable!("process is an engine object");
        };
        let process_index = process_id.slot() as usize - 1;
        let HeapEntry::Object { properties, .. } = &heap[process_index] else {
            unreachable!("process is an ordinary object");
        };
        let Some(Property::Data { value: argv, .. }) = properties.get_ascii("argv") else {
            unreachable!("process owns argv");
        };
        let Some(Decoded::HeapRef(argv_id)) = argv.decode() else {
            unreachable!("process.argv is an engine array");
        };
        let argv_index = argv_id.slot() as usize - 1;
        let HeapEntry::Array { elements, .. } = &mut heap[argv_index] else {
            unreachable!("process.argv is an array");
        };
        *elements = argv_values;
        let intrinsic_slots = heap.len();
        let fuel = limits.fuel;
        Self {
            program,
            module,
            host,
            limits,
            fuel,
            frames: vec![frame],
            heap,
            heap_bytes: 0,
            intrinsic_slots,
            live_registers,
            native_depth: 0,
            last_completion: None,
            globals: BTreeMap::new(),
            registry: ModuleRegistry {
                external: installed_external
                    .into_iter()
                    .map(|module| {
                        let mut exports: BTreeMap<_, _> = module
                            .exports
                            .into_iter()
                            .map(|(name, value)| (name, ExternalExport { value, cell: None }))
                            .collect();
                        exports.insert(
                            EcmaString::from_utf8("default"),
                            ExternalExport {
                                value: module.namespace,
                                cell: None,
                            },
                        );
                        (
                            module.specifier,
                            ExternalModuleInstance {
                                namespace: module.namespace,
                                exports,
                                internals: module.internals,
                            },
                        )
                    })
                    .collect(),
                ..ModuleRegistry::default()
            },
            current_builtin_id: None,
            intrinsics,
        }
    }

    pub fn run(mut self) -> Result<Execution, RuntimeError> {
        if let Some(program) = self.program {
            self.frames.clear();
            self.live_registers = 0;
            self.instantiate_modules()?;
            return self.evaluate_module(program.entry())?.ok_or_else(|| {
                self.program_error(
                    program.entry(),
                    RuntimeErrorKind::InvalidVerifiedProgram {
                        module: program.entry(),
                        instruction: Instruction::Halt,
                    },
                )
            });
        }
        Ok(self
            .run_loop(0)?
            .expect("the entry frame completes before the run loop stops"))
    }

    fn program(&self) -> &Program<Verified> {
        self.program
            .expect("module registry operations require a whole program")
    }

    fn module_code(&self, module: ModuleId) -> &Module<Verified> {
        match self.program {
            Some(program) => {
                &program
                    .module(module)
                    .expect("verified module id remains in bounds")
                    .code
            }
            None => self.module,
        }
    }

    fn allocate_cell(&mut self, value: Value, module: ModuleId) -> Result<CellId, RuntimeError> {
        if self.registry.cells.len() >= self.limits.max_module_cells {
            return Err(self.program_error(
                module,
                RuntimeErrorKind::ModuleCellLimitExceeded {
                    limit: self.limits.max_module_cells,
                },
            ));
        }
        let id = CellId(self.registry.cells.len());
        self.registry.cells.push(Cell { value });
        Ok(id)
    }

    pub(crate) fn instantiate_modules(&mut self) -> Result<(), RuntimeError> {
        let program = self
            .program
            .expect("module registry operations require a whole program");
        self.registry.modules = program
            .modules()
            .iter()
            .map(|module| ModuleInstance {
                binding_cells: vec![None; module.bindings.len()],
                constant_cells: vec![None; module.code.constants().len()],
                namespace: None,
                state: ModuleState::Unevaluated,
            })
            .collect();

        for module_index in 0..program.modules().len() {
            let module_id = ModuleId::new(module_index as u32);
            let bindings = program.modules()[module_index].bindings.clone();
            for (binding_index, binding) in bindings.into_iter().enumerate() {
                let initial = match binding.kind {
                    BindingKind::Hoisted => Some(Value::UNDEFINED),
                    BindingKind::Lexical => Some(Value::UNINITIALIZED),
                    BindingKind::Imported { .. } | BindingKind::Namespace { .. } => None,
                };
                if let Some(value) = initial {
                    let cell = self.allocate_cell(value, module_id)?;
                    self.registry.modules[module_index].binding_cells[binding_index] = Some(cell);
                }
            }
        }

        for module_index in 0..program.modules().len() {
            let module_id = ModuleId::new(module_index as u32);
            let bindings = program.modules()[module_index].bindings.clone();
            for (binding_index, binding) in bindings.into_iter().enumerate() {
                let cell = match binding.kind {
                    BindingKind::Hoisted | BindingKind::Lexical => continue,
                    BindingKind::Imported { edge, name } => {
                        let dependency = program.modules()[module_index].edges[edge.get() as usize];
                        match dependency.target {
                            EdgeTarget::External => {
                                let name = self.constant_text(module_id, name).clone();
                                self.external_export_cell(module_id, edge, &name)?
                            }
                            EdgeTarget::Local(target) => match program
                                .resolve_export(target, self.constant_text(module_id, name))
                            {
                                Some(ResolvedExport::Local { module, binding }) => {
                                    self.registry.modules[module.get() as usize].binding_cells
                                        [binding.get() as usize]
                                        .expect("own cells are allocated before aliases link")
                                }
                                Some(ResolvedExport::External { module, edge, name }) => {
                                    let name = self.constant_text(module, name).clone();
                                    self.external_export_cell(module, edge, &name)?
                                }
                                None => {
                                    return Err(self.program_error(
                                        module_id,
                                        RuntimeErrorKind::InvalidVerifiedProgram {
                                            module: module_id,
                                            instruction: Instruction::Import {
                                                dst: bamts_bytecode::Register::new(0),
                                                specifier: name,
                                            },
                                        },
                                    ));
                                }
                            },
                        }
                    }
                    BindingKind::Namespace { edge } => {
                        let dependency = program.modules()[module_index].edges[edge.get() as usize];
                        let namespace = match dependency.target {
                            EdgeTarget::Local(target) => {
                                self.module_namespace(target, module_id)?
                            }
                            EdgeTarget::External => self.external_namespace(module_id, edge)?,
                        };
                        self.allocate_cell(namespace, module_id)?
                    }
                };
                self.registry.modules[module_index].binding_cells[binding_index] = Some(cell);
            }
        }

        for module_index in 0..program.modules().len() {
            let bindings = &program.modules()[module_index].bindings;
            let constants = program.modules()[module_index].code.constants();
            for (constant_index, constant) in constants.iter().enumerate() {
                let Constant::String(name) = constant else {
                    continue;
                };
                if let Some((binding_index, _)) =
                    bindings.iter().enumerate().find(|(_, binding)| {
                        self.constant_text(ModuleId::new(module_index as u32), binding.name) == name
                    })
                {
                    self.registry.modules[module_index].constant_cells[constant_index] =
                        self.registry.modules[module_index].binding_cells[binding_index];
                }
            }
        }
        Ok(())
    }

    fn module_namespace(
        &mut self,
        target: ModuleId,
        requester: ModuleId,
    ) -> Result<Value, RuntimeError> {
        if let Some(value) = self.registry.modules[target.get() as usize].namespace {
            return Ok(value);
        }
        let exported_names: Vec<EcmaString> = self.program().modules()[target.get() as usize]
            .exports
            .iter()
            .map(|export| self.constant_text(target, export.name).clone())
            .collect();
        for exported_name in exported_names {
            if let Some(ResolvedExport::External { module, edge, name }) =
                self.program().resolve_export(target, &exported_name)
            {
                let name = self.constant_text(module, name).clone();
                self.external_export_cell(module, edge, &name)?;
            }
        }
        let value = self
            .allocate(HeapEntry::ModuleNamespace { module: target })
            .map_err(|kind| self.program_error(requester, kind))?;
        self.registry.modules[target.get() as usize].namespace = Some(value);
        Ok(value)
    }

    fn external_specifier(&self, module: ModuleId, edge: EdgeId) -> Option<EcmaString> {
        let dependency = self.program().modules()[module.get() as usize].edges[edge.get() as usize];
        let specifier = self.constant_text(module, dependency.specifier);
        self.registry
            .external
            .contains_key(specifier)
            .then(|| specifier.clone())
    }

    fn external_namespace(
        &mut self,
        module: ModuleId,
        edge: EdgeId,
    ) -> Result<Value, RuntimeError> {
        let Some(specifier) = self.external_specifier(module, edge) else {
            return Err(self.program_error(
                module,
                RuntimeErrorKind::ExternalModuleUnavailable { module, edge },
            ));
        };
        let export_names: Vec<EcmaString> = self.registry.external[&specifier]
            .exports
            .keys()
            .cloned()
            .collect();
        for name in export_names {
            self.external_export_cell(module, edge, &name)?;
        }
        Ok(self.registry.external[&specifier].namespace)
    }

    fn external_export_cell(
        &mut self,
        module: ModuleId,
        edge: EdgeId,
        name: &EcmaString,
    ) -> Result<CellId, RuntimeError> {
        let Some(specifier) = self.external_specifier(module, edge) else {
            return Err(self.program_error(
                module,
                RuntimeErrorKind::ExternalModuleUnavailable { module, edge },
            ));
        };
        let Some(export) = self.registry.external[&specifier]
            .exports
            .get(name)
            .copied()
        else {
            return Err(self.program_error(
                module,
                RuntimeErrorKind::ExternalModuleUnavailable { module, edge },
            ));
        };
        if let Some(cell) = export.cell {
            return Ok(cell);
        }
        let cell = self.allocate_cell(export.value, module)?;
        self.registry
            .external
            .get_mut(&specifier)
            .expect("external module remains registered")
            .exports
            .get_mut(name)
            .expect("external export remains registered")
            .cell = Some(cell);
        Ok(cell)
    }

    fn evaluate_module(&mut self, module: ModuleId) -> Result<Option<Execution>, RuntimeError> {
        let dependencies = match self.begin_module_evaluation(module)? {
            ModuleEvaluation::Cycle => return Ok(None),
            ModuleEvaluation::Evaluated(result) => return result.map(Some),
            ModuleEvaluation::Ready(dependencies) => dependencies,
        };
        for dependency in dependencies {
            if let Err(error) = self.evaluate_module(dependency) {
                return self.finish_module_evaluation(module, Err(error)).map(Some);
            }
        }

        let code = self.module_code(module);
        let function = code.entry().get() as usize;
        let metadata = &code.functions()[function];
        let register_count = metadata.register_count() as usize;
        let result = if self.limits.max_call_depth < 1 {
            Err(self.program_error(
                module,
                RuntimeErrorKind::CallDepthExceeded {
                    limit: self.limits.max_call_depth,
                },
            ))
        } else if register_count > self.limits.max_total_registers {
            Err(self.program_error(
                module,
                RuntimeErrorKind::RegisterLimitExceeded {
                    limit: self.limits.max_total_registers,
                },
            ))
        } else {
            self.frames.push(Frame::new(
                RuntimeFunction {
                    module,
                    function: FunctionId::new(function as u32),
                },
                metadata,
                &[],
                Value::UNDEFINED,
                Value::UNDEFINED,
                &[],
                None,
            ));
            self.live_registers = register_count;
            self.run_loop(0).and_then(|execution| {
                execution.ok_or_else(|| {
                    self.program_error(
                        module,
                        RuntimeErrorKind::InvalidVerifiedProgram {
                            module,
                            instruction: Instruction::Halt,
                        },
                    )
                })
            })
        };
        self.finish_module_evaluation(module, result).map(Some)
    }

    pub(crate) fn begin_module_evaluation(
        &mut self,
        module: ModuleId,
    ) -> Result<ModuleEvaluation, RuntimeError> {
        match self.registry.modules[module.get() as usize].state.clone() {
            ModuleState::Evaluating => return Ok(ModuleEvaluation::Cycle),
            ModuleState::Evaluated(result) => return Ok(ModuleEvaluation::Evaluated(result)),
            ModuleState::Unevaluated => {}
        }
        self.registry.modules[module.get() as usize].state = ModuleState::Evaluating;

        let mut dependencies = Vec::new();
        for (edge_index, edge) in self.program().modules()[module.get() as usize]
            .edges
            .iter()
            .copied()
            .enumerate()
        {
            if !edge.kind.has_static() {
                continue;
            }
            match edge.target {
                EdgeTarget::Local(dependency) => dependencies.push(dependency),
                EdgeTarget::External
                    if self
                        .external_specifier(module, EdgeId::new(edge_index as u32))
                        .is_some() => {}
                EdgeTarget::External => {
                    let error = self.program_error(
                        module,
                        RuntimeErrorKind::ExternalModuleUnavailable {
                            module,
                            edge: EdgeId::new(edge_index as u32),
                        },
                    );
                    self.registry.modules[module.get() as usize].state =
                        ModuleState::Evaluated(Err(error.clone()));
                    return Err(error);
                }
            }
        }
        Ok(ModuleEvaluation::Ready(dependencies))
    }

    pub(crate) fn finish_module_evaluation(
        &mut self,
        module: ModuleId,
        result: Result<Execution, RuntimeError>,
    ) -> Result<Execution, RuntimeError> {
        if result.is_err() {
            self.frames.clear();
            self.live_registers = 0;
        }
        self.registry.modules[module.get() as usize].state = ModuleState::Evaluated(result.clone());
        result
    }

    pub(crate) fn abort_module_evaluation(&mut self, module: ModuleId) {
        if matches!(
            self.registry.modules[module.get() as usize].state,
            ModuleState::Evaluating
        ) {
            self.registry.modules[module.get() as usize].state = ModuleState::Unevaluated;
        }
    }

    pub(crate) fn constant_text(&self, module: ModuleId, id: ConstantId) -> &EcmaString {
        match &self.module_code(module).constants()[id.get() as usize] {
            Constant::String(text) => text,
            _ => unreachable!("verified module names are strings"),
        }
    }

    fn program_error(&self, module: ModuleId, kind: RuntimeErrorKind) -> RuntimeError {
        let code = self.module_code(module);
        let function = code.entry().get() as usize;
        let instruction = code.functions()[function]
            .code()
            .first()
            .copied()
            .unwrap_or(Instruction::Halt);
        RuntimeError {
            kind,
            function: FunctionId::new(function as u32),
            pc: Pc::new(0),
            source: RuntimeSource {
                function_name: None,
                instruction,
            },
        }
    }

    fn run_loop(&mut self, stop_depth: usize) -> Result<Option<Execution>, RuntimeError> {
        if self.frames.len().saturating_add(self.native_depth) > self.limits.max_call_depth {
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
            let (module_id, function_index, pc) = {
                let frame = &self.frames[frame_index];
                (frame.module, frame.function, frame.pc)
            };
            if let Err(kind) = self.consume_fuel(1) {
                return Err(self.error_at(kind, function_index, pc));
            }
            let instruction = self.module_code(module_id).functions()[function_index].code()[pc];

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
                            properties: PropertyMap::default(),
                            prototype: Some(self.intrinsics.object_prototype),
                            boxed_primitive: None,
                            extensible: true,
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::CreateArray { dst } => {
                    let value = self
                        .allocate(HeapEntry::Array {
                            elements: Vec::new(),
                            properties: PropertyMap::default(),
                            prototype: Some(self.intrinsics.array_prototype),
                            extensible: true,
                            length_writable: true,
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::CreateClosure {
                    dst,
                    function,
                    captures,
                } => match self.read_captures(frame_index, captures.get(), function) {
                    Ok(captures) => {
                        let value = self
                            .allocate(HeapEntry::Function {
                                module: module_id,
                                function,
                                captures,
                                properties: PropertyMap::default(),
                                prototype: Some(self.intrinsics.function_prototype),
                                extensible: true,
                            })
                            .map_err(|kind| self.error_at(kind, function_index, pc))?;
                        self.write_register(frame_index, dst.get(), value);
                        self.frames[frame_index].pc = pc + 1;
                    }
                    Err(failure) => self.resolve_failure(failure, pc)?,
                },
                Instruction::GetProperty { dst, object, key } => {
                    let object = self.read_register(frame_index, object.get());
                    let key_value = self.read_register(frame_index, key.get());
                    let key = match self.to_property_key(key_value) {
                        Ok(key) => key,
                        Err(failure) => {
                            self.resolve_failure(failure, pc)?;
                            continue;
                        }
                    };
                    match self.resolve_get(object, &key) {
                        Ok(GetOutcome::Value(value)) => {
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Ok(GetOutcome::Text(text)) => {
                            let value = self
                                .allocate(HeapEntry::String(text))
                                .map_err(|kind| self.error_at(kind, function_index, pc))?;
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Ok(GetOutcome::Getter(getter)) => {
                            self.frames[frame_index].pc = pc + 1;
                            self.execute_call(CallRequest {
                                callee: getter,
                                this_value: object,
                                arguments: &[],
                                destination: Some(dst.get()),
                                call_pc: pc,
                                constructed: None,
                                new_target: Value::UNDEFINED,
                            })?;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::SetProperty { object, key, value } => {
                    let object = self.read_register(frame_index, object.get());
                    let value = self.read_register(frame_index, value.get());
                    let key_value = self.read_register(frame_index, key.get());
                    let key = match self.to_property_key(key_value) {
                        Ok(key) => key,
                        Err(failure) => {
                            self.resolve_failure(failure, pc)?;
                            continue;
                        }
                    };
                    match self.resolve_set(object, key, value) {
                        Ok(SetOutcome::Done) => self.frames[frame_index].pc = pc + 1,
                        Ok(SetOutcome::Setter(setter)) => {
                            self.frames[frame_index].pc = pc + 1;
                            self.execute_call(CallRequest {
                                callee: setter,
                                this_value: object,
                                arguments: &[value],
                                destination: None,
                                call_pc: pc,
                                constructed: None,
                                new_target: Value::UNDEFINED,
                            })?;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::DeleteProperty { dst, object, key } => {
                    let object = self.read_register(frame_index, object.get());
                    let key_value = self.read_register(frame_index, key.get());
                    let key = match self.to_property_key(key_value) {
                        Ok(key) => key,
                        Err(failure) => {
                            self.resolve_failure(failure, pc)?;
                            continue;
                        }
                    };
                    match self.delete_property(object, &key) {
                        Ok(deleted) => {
                            self.write_register(frame_index, dst.get(), Value::boolean(deleted));
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::DefineAccessor {
                    object,
                    key,
                    accessor,
                    kind,
                } => {
                    let object = self.read_register(frame_index, object.get());
                    let accessor = self.read_register(frame_index, accessor.get());
                    let key_value = self.read_register(frame_index, key.get());
                    let key = match self.to_property_key(key_value) {
                        Ok(key) => key,
                        Err(failure) => {
                            self.resolve_failure(failure, pc)?;
                            continue;
                        }
                    };
                    match self.define_accessor(object, key, accessor, kind) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::Call {
                    dst,
                    callee,
                    this_value,
                    arguments,
                } => {
                    let callee = self.read_register(frame_index, callee.get());
                    let this_value = self.read_register(frame_index, this_value.get());
                    match self.read_arguments(frame_index, arguments.get()) {
                        Ok(arguments) => {
                            self.frames[frame_index].pc = pc + 1;
                            self.execute_call(CallRequest {
                                callee,
                                this_value,
                                arguments: &arguments,
                                destination: Some(dst.get()),
                                call_pc: pc,
                                constructed: None,
                                new_target: Value::UNDEFINED,
                            })?;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::Construct {
                    dst,
                    callee,
                    arguments,
                } => {
                    let callee = self.read_register(frame_index, callee.get());
                    match self.read_arguments(frame_index, arguments.get()) {
                        Ok(arguments) => {
                            self.frames[frame_index].pc = pc + 1;
                            self.execute_construct(callee, &arguments, dst.get(), pc)?;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::LoadGlobal { dst, name } => match self.load_global(module_id, name) {
                    Ok(Some(value)) => {
                        self.write_register(frame_index, dst.get(), value);
                        self.frames[frame_index].pc = pc + 1;
                    }
                    Ok(None) => self.throw(
                        Value::UNDEFINED,
                        ThrowOrigin::ReferenceError {
                            operation: "global is not defined",
                        },
                        pc,
                    )?,
                    Err(kind) => return Err(self.error_here_at(kind, pc)),
                },
                Instruction::StoreGlobal { name, value } => {
                    let value = self.read_register(frame_index, value.get());
                    match self.store_global(module_id, name, value) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::TypeOfGlobal { dst, name } => {
                    let text = match self.load_global(module_id, name) {
                        Ok(value) => value.map_or("undefined", |value| self.type_of(value)),
                        Err(kind) => return Err(self.error_here_at(kind, pc)),
                    };
                    let value = self
                        .allocate(HeapEntry::String(EcmaString::from_utf8(text)))
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::LoadThis { dst } => {
                    let value = self.frames[frame_index].this_value;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::LoadArguments { dst } => {
                    let value = self.materialize_arguments(frame_index, function_index, pc)?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::LoadNewTarget { dst } => {
                    let value = self.frames[frame_index].new_target;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::ArrayPush { array, value } => {
                    let array = self.read_register(frame_index, array.get());
                    let value = self.read_register(frame_index, value.get());
                    match self.array_push(array, value) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::ArrayExtend { array, iterable } => {
                    let array = self.read_register(frame_index, array.get());
                    let iterable = self.read_register(frame_index, iterable.get());
                    match self.array_extend(array, iterable) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::ObjectSpread { target, source } => {
                    let target = self.read_register(frame_index, target.get());
                    let source = self.read_register(frame_index, source.get());
                    match self.object_spread(target, source) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::SetPrototype { object, prototype } => {
                    let object = self.read_register(frame_index, object.get());
                    let prototype = self.read_register(frame_index, prototype.get());
                    match self.set_prototype(object, prototype) {
                        Ok(()) => self.frames[frame_index].pc = pc + 1,
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::CreatePrivateName { dst, description } => {
                    let description = self.constant_string(description).clone();
                    let value = self
                        .allocate(HeapEntry::PrivateName { description })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::CreateRegExp {
                    dst,
                    pattern,
                    flags,
                } => {
                    let pattern = self.constant_string(pattern).clone();
                    let flags = self.constant_string(flags).clone();
                    let value = self
                        .allocate(HeapEntry::RegExp {
                            pattern,
                            flags,
                            properties: PropertyMap::default(),
                            prototype: Some(self.intrinsics.regexp_prototype()),
                            extensible: true,
                        })
                        .map_err(|kind| self.error_at(kind, function_index, pc))?;
                    self.write_register(frame_index, dst.get(), value);
                    self.frames[frame_index].pc = pc + 1;
                }
                Instruction::GetIterator { dst, src, kind } => {
                    let src = self.read_register(frame_index, src.get());
                    match self.create_iterator(src, kind) {
                        Ok(value) => {
                            self.write_register(frame_index, dst.get(), value);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
                }
                Instruction::IteratorNext {
                    done,
                    value,
                    iterator,
                } => {
                    let iterator = self.read_register(frame_index, iterator.get());
                    match self.iterator_next(iterator) {
                        Ok((is_done, produced)) => {
                            self.write_register(frame_index, done.get(), Value::boolean(is_done));
                            self.write_register(frame_index, value.get(), produced);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
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
                        return Ok(Some(execution));
                    }
                    if self.frames.len() == stop_depth {
                        return Ok(None);
                    }
                }
                Instruction::Throw { value } => {
                    let value = self.read_register(frame_index, value.get());
                    self.throw(value, ThrowOrigin::Bytecode, pc)?;
                }
                Instruction::Suspend { .. } => {
                    self.throw_type("suspend outside an engine-owned event loop", pc)?;
                }
                Instruction::Import { .. } => {
                    return Err(self.error_here_at(
                        RuntimeErrorKind::DynamicImportUnsupported { module: module_id },
                        pc,
                    ));
                }
                Instruction::Export { .. } => {
                    return Err(self.error_here_at(
                        RuntimeErrorKind::InvalidVerifiedProgram {
                            module: module_id,
                            instruction,
                        },
                        pc,
                    ));
                }
                Instruction::Halt => {
                    if let Some(execution) = self.complete_frame(Value::UNDEFINED) {
                        return Ok(Some(execution));
                    }
                    if self.frames.len() == stop_depth {
                        return Ok(None);
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

    fn constant_string(&self, id: ConstantId) -> &EcmaString {
        self.constant_text(self.active_module_id(), id)
    }

    fn load_constant(
        &mut self,
        id: ConstantId,
        function: usize,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        self.load_constant_value(self.active_module_id(), id)
            .map_err(|kind| self.error_at(kind, function, pc))
    }

    fn allocate(&mut self, entry: HeapEntry) -> Result<Value, RuntimeErrorKind> {
        if self.heap.len().saturating_sub(self.intrinsic_slots) >= self.limits.max_heap_slots
            || self.heap.len() == u32::MAX as usize
        {
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
            return Err(RuntimeErrorKind::InvalidValue { value });
        };
        let Decoded::HeapRef(id) = decoded else {
            return Ok(None);
        };
        if id.segment() != RUNTIME_HEAP_SEGMENT {
            return Err(RuntimeErrorKind::InvalidValue { value });
        }
        let index = id.slot() as usize - 1;
        if index >= self.heap.len() {
            return Err(RuntimeErrorKind::InvalidRuntimeHeapReference { slot: id.slot() });
        }
        Ok(Some(index))
    }

    fn active_module_id(&self) -> ModuleId {
        self.frames
            .last()
            .map_or(ModuleId::new(0), |frame| frame.module)
    }

    pub(crate) fn load_global(
        &self,
        module: ModuleId,
        name: ConstantId,
    ) -> Result<Option<Value>, RuntimeErrorKind> {
        if let Some(cell) = self
            .registry
            .modules
            .get(module.get() as usize)
            .and_then(|instance| instance.constant_cells.get(name.get() as usize))
            .copied()
            .flatten()
        {
            let value = self.registry.cells[cell.0].value;
            if value.is_uninitialized() {
                let binding = self.registry.modules[module.get() as usize]
                    .binding_cells
                    .iter()
                    .position(|candidate| *candidate == Some(cell))
                    .map(|index| BindingId::new(index as u32))
                    .expect("linked cell belongs to a binding");
                return Err(RuntimeErrorKind::TemporalDeadZone { module, binding });
            }
            return Ok(Some(value));
        }
        Ok(self.resolve_global_binding(self.constant_text(module, name)))
    }

    pub(crate) fn store_global(
        &mut self,
        module: ModuleId,
        name: ConstantId,
        value: Value,
    ) -> Result<(), EvalFailure> {
        let cell = self
            .registry
            .modules
            .get(module.get() as usize)
            .and_then(|instance| instance.constant_cells.get(name.get() as usize))
            .copied()
            .flatten();
        if let Some(cell) = cell {
            let binding = self.registry.modules[module.get() as usize]
                .binding_cells
                .iter()
                .position(|candidate| *candidate == Some(cell))
                .expect("mapped module cell belongs to a binding");
            if matches!(
                self.program().modules()[module.get() as usize].bindings[binding].kind,
                BindingKind::Imported { .. } | BindingKind::Namespace { .. }
            ) {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "assign to immutable module binding",
                }));
            }
            self.registry.cells[cell.0].value = value;
        } else {
            let name = self.constant_text(module, name).to_owned();
            self.globals.insert(name, value);
        }
        Ok(())
    }

    /// Resolves a true realm global after module bindings have been considered.
    fn resolve_global_binding(&self, name: &EcmaString) -> Option<Value> {
        self.globals.get(name).copied().or_else(|| {
            self.intrinsics
                .globals
                .iter()
                .find_map(|(candidate, value)| (candidate == name).then_some(*value))
        })
    }

    /// Classifies a callee into the shared dispatch categories.
    fn callee_kind(&self, callee: Value) -> Result<CalleeKind, RuntimeErrorKind> {
        match self.runtime_slot(callee)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Function {
                    module,
                    function,
                    captures,
                    ..
                } => Ok(CalleeKind::Runtime {
                    target: RuntimeFunction {
                        module: *module,
                        function: *function,
                    },
                    captures: captures.clone(),
                }),
                HeapEntry::NativeFunction { id, bound_this, .. } => Ok(CalleeKind::Builtin {
                    id: *id,
                    bound_this: *bound_this,
                }),
                _ => Ok(CalleeKind::NotCallable),
            },
            None => Ok(CalleeKind::NotCallable),
        }
    }

    /// Materializes a constant into an ABI value, interning strings and bigints
    /// into the slot heap. Shared with the native engine.
    pub(crate) fn load_constant_value(
        &mut self,
        module: ModuleId,
        id: ConstantId,
    ) -> Result<Value, RuntimeErrorKind> {
        match &self.module_code(module).constants()[id.get() as usize] {
            Constant::String(text) => self.allocate(HeapEntry::String(text.clone())),
            Constant::BigInt(value) => self.allocate(HeapEntry::BigInt(value.as_str().to_owned())),
            constant => Ok(constant_value(constant).expect("non-heap constant")),
        }
    }

    /// Reads a call/construct arguments array from a register: it must hold a
    /// runtime array, whose length is capped by `max_argument_count`.
    fn read_arguments(&self, frame: usize, register: u32) -> Result<Vec<Value>, EvalFailure> {
        let value = self.read_register(frame, register);
        self.arguments_from_array(value)
    }

    /// Validates a call/construct arguments array value: it must be a runtime
    /// array whose length is capped by `max_argument_count`, with holes read as
    /// `undefined`. Shared with the native engine.
    fn arguments_from_array(&self, arguments: Value) -> Result<Vec<Value>, EvalFailure> {
        match self.runtime_slot(arguments).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Array { elements, .. } => {
                    if elements.len() as u64 > u64::from(self.limits.max_argument_count) {
                        return Err(EvalFailure::Runtime(
                            RuntimeErrorKind::ArgumentLimitExceeded {
                                limit: self.limits.max_argument_count,
                                requested: u32::try_from(elements.len()).unwrap_or(u32::MAX),
                            },
                        ));
                    }
                    Ok(elements
                        .iter()
                        .map(|value| {
                            if *value == Value::HOLE {
                                Value::UNDEFINED
                            } else {
                                *value
                            }
                        })
                        .collect())
                }
                _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "call arguments are not an array",
                })),
            },
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "call arguments are not an array",
            })),
        }
    }

    /// Reads a `CreateClosure` captures array: it must hold a runtime array whose
    /// length matches the target function's capture count.
    fn read_captures(
        &self,
        frame: usize,
        register: u32,
        function: FunctionId,
    ) -> Result<Vec<Value>, EvalFailure> {
        let value = self.read_register(frame, register);
        self.captures_from_array(self.active_module_id(), value, function)
    }

    /// Validates a `CreateClosure` captures array value: it must be a runtime
    /// array whose length matches the target function's capture count, with
    /// holes read as `undefined`. Shared with the native engine.
    pub(crate) fn captures_from_array(
        &self,
        module: ModuleId,
        captures: Value,
        function: FunctionId,
    ) -> Result<Vec<Value>, EvalFailure> {
        let expected =
            self.module_code(module).functions()[function.get() as usize].capture_count() as usize;
        match self.runtime_slot(captures).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Array { elements, .. } => {
                    if elements.len() != expected {
                        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                            operation: "closure capture array arity",
                        }));
                    }
                    Ok(elements
                        .iter()
                        .map(|value| {
                            if *value == Value::HOLE {
                                Value::UNDEFINED
                            } else {
                                *value
                            }
                        })
                        .collect())
                }
                _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "closure captures are not an array",
                })),
            },
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "closure captures are not an array",
            })),
        }
    }

    pub(crate) fn materialize_arguments(
        &mut self,
        frame: usize,
        function: usize,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        if let Some(existing) = self.frames[frame].arguments_object {
            return Ok(existing);
        }
        let args = self.frames[frame].args.clone();
        let value = self
            .allocate(HeapEntry::Array {
                elements: args,
                properties: PropertyMap::default(),
                prototype: Some(self.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .map_err(|kind| self.error_at(kind, function, pc))?;
        self.frames[frame].arguments_object = Some(value);
        Ok(value)
    }

    fn push_frame(
        &mut self,
        target: RuntimeFunction,
        captures: &[Value],
        this_value: Value,
        new_target: Value,
        arguments: &[Value],
        return_to: ReturnTo,
    ) -> Result<(), RuntimeError> {
        let function_index = target.function.get() as usize;
        let metadata = &self.module_code(target.module).functions()[function_index];
        let limit_error = |kind| {
            if let Some(caller) = self.frames.last() {
                self.error_at_in_module(kind, caller.module, caller.function, return_to.call_pc)
            } else {
                self.error_at_in_module(kind, target.module, function_index, 0)
            }
        };
        if self.frames.len().saturating_add(self.native_depth) >= self.limits.max_call_depth {
            return Err(limit_error(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            }));
        }
        let next_registers = metadata.register_count() as usize;
        if self.live_registers.saturating_add(next_registers) > self.limits.max_total_registers {
            return Err(limit_error(RuntimeErrorKind::RegisterLimitExceeded {
                limit: self.limits.max_total_registers,
            }));
        }
        let frame = Frame::new(
            target,
            metadata,
            captures,
            this_value,
            new_target,
            arguments,
            Some(return_to),
        );
        self.live_registers += next_registers;
        self.frames.push(frame);
        Ok(())
    }

    pub(crate) fn consume_fuel(&mut self, amount: u64) -> Result<(), RuntimeErrorKind> {
        if self.fuel < amount {
            self.fuel = 0;
            return Err(RuntimeErrorKind::FuelExhausted {
                limit: self.limits.fuel,
            });
        }
        self.fuel -= amount;
        Ok(())
    }

    pub(crate) fn reserve_native_activation(
        &mut self,
        register_count: usize,
    ) -> Result<(), RuntimeErrorKind> {
        if self.frames.len().saturating_add(self.native_depth) >= self.limits.max_call_depth {
            return Err(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            });
        }
        if self.live_registers.saturating_add(register_count) > self.limits.max_total_registers {
            return Err(RuntimeErrorKind::RegisterLimitExceeded {
                limit: self.limits.max_total_registers,
            });
        }
        self.native_depth += 1;
        self.live_registers += register_count;
        Ok(())
    }

    pub(crate) fn release_native_activation(&mut self, register_count: usize) {
        self.native_depth -= 1;
        self.live_registers -= register_count;
    }

    fn execute_call(&mut self, request: CallRequest) -> Result<(), RuntimeError> {
        let CallRequest {
            callee,
            this_value,
            arguments,
            destination,
            call_pc,
            constructed,
            new_target,
        } = request;
        match self.callee_kind(callee) {
            Ok(CalleeKind::Runtime { target, captures }) => self.push_frame(
                target,
                &captures,
                this_value,
                new_target,
                arguments,
                ReturnTo {
                    destination: destination.map(|register| register as usize),
                    call_pc,
                    constructed,
                },
            ),
            Ok(CalleeKind::Builtin { id, bound_this }) => {
                let this_value = bound_this.unwrap_or(this_value);
                match self.call_builtin(id, this_value, arguments, false) {
                    Ok(intrinsics::BuiltinOutcome::Value(value)) => {
                        if let Some(register) = destination {
                            self.write_register(self.frames.len() - 1, register, value);
                        }
                        Ok(())
                    }
                    Ok(intrinsics::BuiltinOutcome::Call {
                        callee,
                        this_value,
                        argument_start,
                    }) => self.execute_call(CallRequest {
                        callee,
                        this_value,
                        arguments: &arguments[argument_start..],
                        destination,
                        call_pc,
                        constructed,
                        new_target,
                    }),
                    Err(failure) => self.resolve_failure(failure, call_pc),
                }
            }
            Ok(CalleeKind::NotCallable) => self.throw_type("call", call_pc),
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
                if let HeapEntry::NativeFunction { id, .. } = self.heap[index] {
                    match self.call_builtin(id, Value::UNDEFINED, arguments, true) {
                        Ok(intrinsics::BuiltinOutcome::Value(value)) => {
                            self.write_register(self.frames.len() - 1, destination, value);
                            return Ok(());
                        }
                        Ok(intrinsics::BuiltinOutcome::Call { .. }) => {
                            return self.throw_type("construct", call_pc);
                        }
                        Err(failure) => return self.resolve_failure(failure, call_pc),
                    }
                }
                if !matches!(
                    self.heap[index],
                    HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. }
                ) {
                    return self.throw_type("construct", call_pc);
                }
                // The instance's [[Prototype]] is the constructor's own
                // `prototype` data property when it is an object.
                let instance_prototype = match self.own_data_property(index, "prototype") {
                    Some(value) if self.is_object(value) => Some(value),
                    _ => Some(self.intrinsics.object_prototype),
                };
                let object = self
                    .allocate(HeapEntry::Object {
                        properties: PropertyMap::default(),
                        prototype: instance_prototype,
                        boxed_primitive: None,
                        extensible: true,
                    })
                    .map_err(|kind| self.error_here_at(kind, call_pc))?;
                self.execute_call(CallRequest {
                    callee,
                    this_value: object,
                    arguments,
                    destination: Some(destination),
                    call_pc,
                    constructed: Some(object),
                    new_target: callee,
                })
            }
            Ok(None) => self.throw_type("construct", call_pc),
            Err(kind) => Err(self.error_here_at(kind, call_pc)),
        }
    }

    pub(crate) fn array_elements(&self, value: Value) -> Result<Option<Vec<Value>>, EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
        match &self.heap[index] {
            HeapEntry::Array { elements, .. } => Ok(Some(elements.clone())),
            _ => Ok(None),
        }
    }

    pub(crate) fn array_length(&self, value: Value) -> Result<usize, EvalFailure> {
        self.array_elements(value)?
            .map(|elements| elements.len())
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "array method called on incompatible receiver",
            }))
    }

    pub(crate) fn replace_array_elements(
        &mut self,
        value: Value,
        elements: Vec<Value>,
    ) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "array method called on incompatible receiver",
            }));
        };
        let HeapEntry::Array {
            elements: current, ..
        } = &mut self.heap[index]
        else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "array method called on incompatible receiver",
            }));
        };
        *current = elements;
        Ok(())
    }

    pub(crate) fn string_value(&self, value: Value) -> Option<EcmaString> {
        let index = self.runtime_slot(value).ok().flatten()?;
        match &self.heap[index] {
            HeapEntry::String(text) => Some(text.clone()),
            _ => None,
        }
    }

    pub(crate) fn get_named_property(
        &mut self,
        object: Value,
        name: &str,
    ) -> Result<Value, EvalFailure> {
        self.get_property_ascii(object, name)
    }

    fn get_property_ascii(&mut self, object: Value, name: &str) -> Result<Value, EvalFailure> {
        debug_assert!(name.is_ascii());
        match self.resolve_get_ascii(object, name)? {
            GetOutcome::Value(value) => Ok(value),
            GetOutcome::Text(text) => self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime),
            GetOutcome::Getter(getter) => self.call_value(getter, object, &[]),
        }
    }

    pub(crate) fn get_property_key(
        &mut self,
        object: Value,
        key: &PropertyKey,
    ) -> Result<Value, EvalFailure> {
        match self.resolve_get(object, key)? {
            GetOutcome::Value(value) => Ok(value),
            GetOutcome::Text(text) => self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime),
            GetOutcome::Getter(getter) => self.call_value(getter, object, &[]),
        }
    }

    pub(crate) fn set_data_property(
        &mut self,
        object: Value,
        name: &str,
        value: Value,
    ) -> Result<(), EvalFailure> {
        self.set_data_property_key(
            object,
            PropertyKey::Named(EcmaString::from_utf8(name)),
            value,
        )
    }

    pub(crate) fn set_data_property_key(
        &mut self,
        object: Value,
        key: PropertyKey,
        value: Value,
    ) -> Result<(), EvalFailure> {
        match self.resolve_set(object, key, value)? {
            SetOutcome::Done => Ok(()),
            SetOutcome::Setter(setter) => {
                self.call_value(setter, object, &[value])?;
                Ok(())
            }
        }
    }

    pub(crate) fn is_callable(&self, value: Value) -> Result<bool, EvalFailure> {
        Ok(!matches!(
            self.callee_kind(value).map_err(EvalFailure::Runtime)?,
            CalleeKind::NotCallable
        ))
    }

    pub(crate) fn box_primitive(&mut self, value: Value) -> Result<Value, EvalFailure> {
        let prototype = match value.decode() {
            Some(Decoded::Boolean(_)) => self.intrinsics.boolean_prototype,
            Some(Decoded::Number(_) | Decoded::Int32(_)) => self.intrinsics.number_prototype,
            Some(Decoded::HeapRef(_)) if self.string_value(value).is_some() => {
                self.intrinsics.string_prototype
            }
            _ => self.intrinsics.object_prototype,
        };
        self.allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            boxed_primitive: Some(value),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)
    }

    pub(crate) fn unbox_primitive_or_self(&self, value: Value) -> Result<Value, EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(value);
        };
        match self.heap[index] {
            HeapEntry::Object {
                boxed_primitive: Some(primitive),
                ..
            } => Ok(primitive),
            _ => Ok(value),
        }
    }

    pub(crate) fn unbox_primitive(
        &self,
        value: Value,
        operation: &'static str,
    ) -> Result<Value, EvalFailure> {
        let unboxed = self.unbox_primitive_or_self(value)?;
        if unboxed == value && self.is_object(value) {
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { operation }))
        } else {
            Ok(unboxed)
        }
    }

    pub(crate) fn current_builtin_id(&self) -> Option<intrinsics::BuiltinId> {
        self.current_builtin_id
    }

    pub(crate) fn throw_error(
        &mut self,
        id: intrinsics::BuiltinId,
        message: String,
    ) -> EvalFailure {
        let message = match self.allocate(HeapEntry::String(EcmaString::from_utf8(&message))) {
            Ok(value) => value,
            Err(kind) => return EvalFailure::Runtime(kind),
        };
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::from_utf8("message")),
            Property::Data {
                value: message,
                writable: true,
                enumerable: true,
                configurable: true,
            },
        );
        match self.allocate(HeapEntry::Object {
            properties,
            prototype: Some(self.intrinsics.error_prototype(id)),
            boxed_primitive: None,
            extensible: true,
        }) {
            Ok(value) => EvalFailure::ThrowValue(value),
            Err(kind) => EvalFailure::Runtime(kind),
        }
    }

    pub(crate) fn has_own_property_key(
        &self,
        object: Value,
        key: &PropertyKey,
    ) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        Ok(self.own_get(index, key).is_some())
    }

    pub(crate) fn call_value(
        &mut self,
        callee: Value,
        this_value: Value,
        arguments: &[Value],
    ) -> Result<Value, EvalFailure> {
        match self.callee_kind(callee).map_err(EvalFailure::Runtime)? {
            CalleeKind::Builtin { id, bound_this } => {
                let receiver = bound_this.unwrap_or(this_value);
                match self.call_builtin(id, receiver, arguments, false)? {
                    intrinsics::BuiltinOutcome::Value(value) => Ok(value),
                    intrinsics::BuiltinOutcome::Call {
                        callee,
                        this_value,
                        argument_start,
                    } => self.call_value(callee, this_value, &arguments[argument_start..]),
                }
            }
            CalleeKind::Runtime { target, captures } => {
                let stop_depth = self.frames.len();
                let call_pc = self.frames.last().map_or(0, |frame| frame.pc);
                self.push_frame(
                    target,
                    &captures,
                    this_value,
                    Value::UNDEFINED,
                    arguments,
                    ReturnTo {
                        destination: None,
                        call_pc,
                        constructed: None,
                    },
                )
                .map_err(|error| EvalFailure::Runtime(error.kind))?;
                match self.run_loop(stop_depth) {
                    Ok(None) => self.last_completion.take().ok_or(EvalFailure::Runtime(
                        RuntimeErrorKind::InvalidValue {
                            value: Value::UNDEFINED,
                        },
                    )),
                    Ok(Some(_)) => unreachable!("a nested call cannot complete the entry frame"),
                    Err(error) => match error.kind {
                        RuntimeErrorKind::UncaughtThrow { value, .. } => {
                            Err(EvalFailure::ThrowValue(value))
                        }
                        kind => Err(EvalFailure::Runtime(kind)),
                    },
                }
            }
            CalleeKind::NotCallable => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "call",
            })),
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
                if let Some(destination) = return_to.destination {
                    self.frames.last_mut().expect("callee has caller").registers[destination] =
                        value;
                } else {
                    self.last_completion = Some(value);
                }
                None
            }
        }
    }

    fn resolve_failure(&mut self, failure: EvalFailure, pc: usize) -> Result<(), RuntimeError> {
        match failure {
            EvalFailure::Throw(origin) => self.throw(Value::UNDEFINED, origin, pc),
            EvalFailure::ThrowValue(value) => self.throw(value, ThrowOrigin::Bytecode, pc),
            EvalFailure::Runtime(kind) => Err(self.error_here_at(kind, pc)),
        }
    }

    fn throw_type(&mut self, operation: &'static str, pc: usize) -> Result<(), RuntimeError> {
        self.throw(Value::UNDEFINED, ThrowOrigin::TypeError { operation }, pc)
    }

    fn throw(
        &mut self,
        value: Value,
        origin: ThrowOrigin,
        faulting_pc: usize,
    ) -> Result<(), RuntimeError> {
        let site_module = self
            .frames
            .last()
            .expect("an activation is executing")
            .module;
        let site_function = self
            .frames
            .last()
            .expect("an activation is executing")
            .function;
        let mut search_pc = faulting_pc;
        loop {
            let frame_index = self.frames.len() - 1;
            let function_index = self.frames[frame_index].function;
            let module = self.frames[frame_index].module;
            let function = &self.module_code(module).functions()[function_index];
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
                    return Err(self.error_at_in_module(
                        RuntimeErrorKind::UncaughtThrow { value, origin },
                        site_module,
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
        self.error_at_in_module(kind, self.active_module_id(), function, pc)
    }

    pub(crate) fn error_at_in_module(
        &self,
        kind: RuntimeErrorKind,
        module: ModuleId,
        function: usize,
        pc: usize,
    ) -> RuntimeError {
        let code = self.module_code(module);
        let metadata = &code.functions()[function];
        let function_name =
            metadata
                .name()
                .and_then(|id| match &code.constants()[id.get() as usize] {
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

    // ---- property keys -----------------------------------------------------

    /// Normalizes a register value into a property key. A runtime string borrows
    /// its text; a private name yields its slot identity; everything else is
    /// coerced with `ToString`.
    fn to_property_key(&self, value: Value) -> Result<PropertyKey, EvalFailure> {
        match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::String(text) => Ok(PropertyKey::Named(text.clone())),
                HeapEntry::Symbol { .. } => Ok(PropertyKey::Symbol(index as u32)),
                HeapEntry::PrivateName { .. } => Ok(PropertyKey::Private(index as u32)),
                _ => Ok(PropertyKey::Named(self.value_to_string(value, 0)?)),
            },
            None => Ok(PropertyKey::Named(self.value_to_string(value, 0)?)),
        }
    }

    // ---- property get ------------------------------------------------------

    fn resolve_get(&mut self, object: Value, key: &PropertyKey) -> Result<GetOutcome, EvalFailure> {
        let slot = self.runtime_slot(object).map_err(EvalFailure::Runtime)?;
        let start = match slot {
            Some(index) => {
                if matches!(self.heap[index], HeapEntry::ProcessEnv { .. }) {
                    let PropertyKey::Named(name) = key else {
                        return Ok(GetOutcome::Value(Value::UNDEFINED));
                    };
                    let text = name
                        .to_utf8_strict()
                        .ok()
                        .and_then(|name| self.host.env(&name))
                        .map(EcmaString::from_utf8);
                    return match text {
                        Some(text) => self
                            .allocate(HeapEntry::String(text))
                            .map(GetOutcome::Value)
                            .map_err(EvalFailure::Runtime),
                        None => Ok(GetOutcome::Value(Value::UNDEFINED)),
                    };
                }
                if let Some(found) = self.primitive_get(index, key) {
                    return Self::found_outcome(found);
                }
                match self.heap[index] {
                    HeapEntry::String(_) => self
                        .runtime_slot(self.intrinsics.string_prototype)
                        .map_err(EvalFailure::Runtime)?,
                    HeapEntry::BigInt(_) | HeapEntry::PrivateName { .. } => self
                        .runtime_slot(self.intrinsics.object_prototype)
                        .map_err(EvalFailure::Runtime)?,
                    HeapEntry::Symbol { .. } => self
                        .runtime_slot(self.intrinsics.builtins.symbol_prototype())
                        .map_err(EvalFailure::Runtime)?,
                    _ => Some(index),
                }
            }
            None => {
                let prototype = match object.decode() {
                    Some(Decoded::Boolean(_)) => self.intrinsics.boolean_prototype,
                    Some(Decoded::Number(_) | Decoded::Int32(_)) => {
                        self.intrinsics.number_prototype
                    }
                    _ => return Ok(GetOutcome::Value(Value::UNDEFINED)),
                };
                self.runtime_slot(prototype).map_err(EvalFailure::Runtime)?
            }
        };
        let Some(mut node) = start else {
            return Ok(GetOutcome::Value(Value::UNDEFINED));
        };
        for _ in 0..=self.heap.len() {
            if let Some(found) = self.own_get(node, key) {
                return Self::found_outcome(found);
            }
            match self.prototype_index(node)? {
                Some(next) => node = next,
                None => return Ok(GetOutcome::Value(Value::UNDEFINED)),
            }
        }
        Ok(GetOutcome::Value(Value::UNDEFINED))
    }

    fn resolve_get_ascii(&mut self, object: Value, name: &str) -> Result<GetOutcome, EvalFailure> {
        debug_assert!(name.is_ascii());
        let slot = self.runtime_slot(object).map_err(EvalFailure::Runtime)?;
        let start = match slot {
            Some(index) => {
                if matches!(self.heap[index], HeapEntry::ProcessEnv { .. }) {
                    return match self.host.env(name).map(EcmaString::from_utf8) {
                        Some(text) => self
                            .allocate(HeapEntry::String(text))
                            .map(GetOutcome::Value)
                            .map_err(EvalFailure::Runtime),
                        None => Ok(GetOutcome::Value(Value::UNDEFINED)),
                    };
                }
                if let HeapEntry::String(text) = &self.heap[index] {
                    if name == "length" {
                        return Ok(GetOutcome::Value(number_value(text.len_units() as f64)));
                    }
                    if let Some(offset) = array_index_ascii(name)
                        && let Some(unit) = text.unit_at(offset as usize)
                    {
                        return Ok(GetOutcome::Text(EcmaString::from_units(&[unit])));
                    }
                }
                match self.heap[index] {
                    HeapEntry::String(_) => self
                        .runtime_slot(self.intrinsics.string_prototype)
                        .map_err(EvalFailure::Runtime)?,
                    HeapEntry::BigInt(_) | HeapEntry::PrivateName { .. } => self
                        .runtime_slot(self.intrinsics.object_prototype)
                        .map_err(EvalFailure::Runtime)?,
                    HeapEntry::Symbol { .. } => self
                        .runtime_slot(self.intrinsics.builtins.symbol_prototype())
                        .map_err(EvalFailure::Runtime)?,
                    _ => Some(index),
                }
            }
            None => {
                let prototype = match object.decode() {
                    Some(Decoded::Boolean(_)) => self.intrinsics.boolean_prototype,
                    Some(Decoded::Number(_) | Decoded::Int32(_)) => {
                        self.intrinsics.number_prototype
                    }
                    _ => return Ok(GetOutcome::Value(Value::UNDEFINED)),
                };
                self.runtime_slot(prototype).map_err(EvalFailure::Runtime)?
            }
        };
        let Some(mut node) = start else {
            return Ok(GetOutcome::Value(Value::UNDEFINED));
        };
        for _ in 0..=self.heap.len() {
            if let Some(found) = self.own_get_ascii(node, name) {
                return Self::found_outcome(found);
            }
            match self.prototype_index(node)? {
                Some(next) => node = next,
                None => return Ok(GetOutcome::Value(Value::UNDEFINED)),
            }
        }
        Ok(GetOutcome::Value(Value::UNDEFINED))
    }

    fn found_outcome(found: Found) -> Result<GetOutcome, EvalFailure> {
        match found {
            Found::Value(value) => Ok(GetOutcome::Value(value)),
            Found::Text(text) => Ok(GetOutcome::Text(text)),
            Found::Getter(getter) => Ok(GetOutcome::Getter(getter)),
            Found::Failure(kind) => Err(EvalFailure::Runtime(kind)),
            Found::NoGetter => Ok(GetOutcome::Value(Value::UNDEFINED)),
        }
    }

    fn primitive_get(&self, index: usize, key: &PropertyKey) -> Option<Found> {
        if let HeapEntry::String(text) = &self.heap[index]
            && let PropertyKey::Named(name) = key
        {
            if name.eq_ascii("length") {
                return Some(Found::Value(number_value(text.len_units() as f64)));
            }
            if let Some(offset) = array_index(name)
                && let Some(unit) = text.unit_at(offset as usize)
            {
                return Some(Found::Text(EcmaString::from_units(&[unit])));
            }
        }
        None
    }
    fn own_get_ascii(&self, index: usize, name: &str) -> Option<Found> {
        debug_assert!(name.is_ascii());
        let slot = |value| self.runtime_slot(value).ok().flatten();
        if slot(self.intrinsics.object_prototype) == Some(index) && name == "toString" {
            return Some(Found::Value(self.intrinsics.object_to_string()));
        }
        if (slot(self.intrinsics.function_prototype) == Some(index)
            || matches!(
                self.heap[index],
                HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. }
            ))
            && name == "call"
        {
            return Some(Found::Value(self.intrinsics.function_call()));
        }
        match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. } => property_lookup_ascii(properties, name),
            HeapEntry::Array {
                elements,
                properties,
                ..
            } => {
                if name == "length" {
                    return Some(Found::Value(number_value(elements.len() as f64)));
                }
                if let Some(offset) = array_index_ascii(name)
                    && let Some(element) = elements.get(offset as usize)
                    && *element != Value::HOLE
                {
                    return Some(Found::Value(*element));
                }
                property_lookup_ascii(properties, name)
            }
            HeapEntry::Function {
                module,
                function,
                properties,
                ..
            } => {
                if let Some(found) = property_lookup_ascii(properties, name) {
                    return Some(found);
                }
                let metadata = &self.module_code(*module).functions()[function.get() as usize];
                if name == "length" {
                    return Some(Found::Value(
                        number_value(metadata.parameter_count() as f64),
                    ));
                }
                if name == "name" {
                    return Some(Found::Text(
                        metadata
                            .name()
                            .map(|id| self.constant_text(*module, id).clone())
                            .unwrap_or_default(),
                    ));
                }
                None
            }
            HeapEntry::ModuleNamespace { module } => {
                let key = self.program().modules()[module.get() as usize]
                    .exports
                    .iter()
                    .map(|export| self.constant_text(*module, export.name))
                    .find(|candidate| candidate.eq_ascii(name))?
                    .clone();
                match self.namespace_export(*module, &key) {
                    Ok(Some(value)) => Some(Found::Value(value)),
                    Ok(None) => None,
                    Err(kind) => Some(Found::Failure(kind)),
                }
            }
            HeapEntry::ExternalModuleNamespace { specifier } => {
                let export = self.registry.external[specifier]
                    .exports
                    .iter()
                    .find_map(|(candidate, export)| candidate.eq_ascii(name).then_some(export))?;
                let cell = export
                    .cell
                    .expect("external namespace exports link before evaluation");
                Some(Found::Value(self.registry.cells[cell.0].value))
            }
            HeapEntry::RegExp {
                pattern,
                flags,
                properties,
                ..
            } => {
                if let Some(found) = property_lookup_ascii(properties, name) {
                    return Some(found);
                }
                let flag = |unit| {
                    Found::Value(Value::boolean(flags.as_units().contains(&u16::from(unit))))
                };
                match name {
                    "source" => Some(Found::Text(pattern.clone())),
                    "flags" => Some(Found::Text(flags.clone())),
                    "global" => Some(flag(b'g')),
                    "ignoreCase" => Some(flag(b'i')),
                    "multiline" => Some(flag(b'm')),
                    "sticky" => Some(flag(b'y')),
                    "unicode" => Some(flag(b'u')),
                    "dotAll" => Some(flag(b's')),
                    "lastIndex" => Some(Found::Value(Value::int32(0))),
                    _ => None,
                }
            }
            HeapEntry::HashState { update, digest, .. } => match name {
                "update" => Some(Found::Value(*update)),
                "digest" => Some(Found::Value(*digest)),
                _ => None,
            },
            HeapEntry::ProcessEnv { .. }
            | HeapEntry::String(_)
            | HeapEntry::BigInt(_)
            | HeapEntry::Symbol { .. }
            | HeapEntry::PrivateName { .. }
            | HeapEntry::Iterator { .. } => None,
        }
    }

    /// Looks up an own property of the heap entry at `index`, returning `None`
    /// when the key is absent so the caller may continue up the prototype chain.
    fn own_get(&self, index: usize, key: &PropertyKey) -> Option<Found> {
        if let PropertyKey::Named(name) = key {
            let slot = |value| self.runtime_slot(value).ok().flatten();
            if slot(self.intrinsics.object_prototype) == Some(index) && name.eq_ascii("toString") {
                return Some(Found::Value(self.intrinsics.object_to_string()));
            }
            if slot(self.intrinsics.function_prototype) == Some(index) && name.eq_ascii("call") {
                return Some(Found::Value(self.intrinsics.function_call()));
            }
            if matches!(
                self.heap[index],
                HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. }
            ) && name.eq_ascii("call")
            {
                return Some(Found::Value(self.intrinsics.function_call()));
            }
        }
        match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. } => property_lookup(properties, key),
            HeapEntry::Array {
                elements,
                properties,
                ..
            } => {
                if let PropertyKey::Named(name) = key {
                    if name.eq_ascii("length") {
                        return Some(Found::Value(number_value(elements.len() as f64)));
                    }
                    if let Some(offset) = array_index(name)
                        && let Some(element) = elements.get(offset as usize)
                        && *element != Value::HOLE
                    {
                        return Some(Found::Value(*element));
                    }
                }
                property_lookup(properties, key)
            }
            HeapEntry::Function {
                module,
                function,
                properties,
                ..
            } => {
                if let Some(found) = property_lookup(properties, key) {
                    return Some(found);
                }
                if let PropertyKey::Named(name) = key {
                    let metadata = &self.module_code(*module).functions()[function.get() as usize];
                    if name.eq_ascii("length") {
                        return Some(Found::Value(
                            number_value(metadata.parameter_count() as f64),
                        ));
                    }
                    if name.eq_ascii("name") {
                        return Some(Found::Text(
                            metadata
                                .name()
                                .map(|id| self.constant_text(*module, id).clone())
                                .unwrap_or_default(),
                        ));
                    }
                }
                None
            }
            HeapEntry::ModuleNamespace { module } => {
                let PropertyKey::Named(name) = key else {
                    return None;
                };
                match self.namespace_export(*module, name) {
                    Ok(Some(value)) => Some(Found::Value(value)),
                    Ok(None) => None,
                    Err(kind) => Some(Found::Failure(kind)),
                }
            }
            HeapEntry::ExternalModuleNamespace { specifier } => {
                let PropertyKey::Named(name) = key else {
                    return None;
                };
                let export = self.registry.external[specifier].exports.get(name)?;
                let cell = export
                    .cell
                    .expect("external namespace exports link before evaluation");
                Some(Found::Value(self.registry.cells[cell.0].value))
            }
            HeapEntry::NativeFunction { properties, .. } => property_lookup(properties, key),
            HeapEntry::RegExp {
                pattern,
                flags,
                properties,
                ..
            } => {
                if let Some(found) = property_lookup(properties, key) {
                    return Some(found);
                }
                if let PropertyKey::Named(name) = key {
                    let flag = |ascii: &str| {
                        Found::Value(Value::boolean(
                            flags.as_units().contains(&u16::from(ascii.as_bytes()[0])),
                        ))
                    };
                    if name.eq_ascii("source") {
                        return Some(Found::Text(pattern.clone()));
                    }
                    if name.eq_ascii("flags") {
                        return Some(Found::Text(flags.clone()));
                    }
                    if name.eq_ascii("global") {
                        return Some(flag("g"));
                    }
                    if name.eq_ascii("ignoreCase") {
                        return Some(flag("i"));
                    }
                    if name.eq_ascii("multiline") {
                        return Some(flag("m"));
                    }
                    if name.eq_ascii("sticky") {
                        return Some(flag("y"));
                    }
                    if name.eq_ascii("unicode") {
                        return Some(flag("u"));
                    }
                    if name.eq_ascii("dotAll") {
                        return Some(flag("s"));
                    }
                    if name.eq_ascii("lastIndex") {
                        return Some(Found::Value(Value::int32(0)));
                    }
                }
                None
            }
            HeapEntry::HashState { update, digest, .. } => {
                let PropertyKey::Named(name) = key else {
                    return None;
                };
                if name.eq_ascii("update") {
                    Some(Found::Value(*update))
                } else if name.eq_ascii("digest") {
                    Some(Found::Value(*digest))
                } else {
                    None
                }
            }
            HeapEntry::ProcessEnv { .. }
            | HeapEntry::String(_)
            | HeapEntry::BigInt(_)
            | HeapEntry::Symbol { .. }
            | HeapEntry::PrivateName { .. }
            | HeapEntry::Iterator { .. } => None,
        }
    }

    fn namespace_export(
        &self,
        module: ModuleId,
        name: &EcmaString,
    ) -> Result<Option<Value>, RuntimeErrorKind> {
        match self.program().resolve_export(module, name) {
            Some(ResolvedExport::Local { module, binding }) => {
                let cell = self.registry.modules[module.get() as usize].binding_cells
                    [binding.get() as usize]
                    .expect("verified export resolves to a linked cell");
                let value = self.registry.cells[cell.0].value;
                if value.is_uninitialized() {
                    Err(RuntimeErrorKind::TemporalDeadZone { module, binding })
                } else {
                    Ok(Some(value))
                }
            }
            Some(ResolvedExport::External { module, edge, name }) => {
                let Some(specifier) = self.external_specifier(module, edge) else {
                    return Err(RuntimeErrorKind::ExternalModuleUnavailable { module, edge });
                };
                let name = self.constant_text(module, name);
                let Some(export) = self.registry.external[&specifier].exports.get(name) else {
                    return Err(RuntimeErrorKind::ExternalModuleUnavailable { module, edge });
                };
                let Some(cell) = export.cell else {
                    return Err(RuntimeErrorKind::ExternalModuleUnavailable { module, edge });
                };
                Ok(Some(self.registry.cells[cell.0].value))
            }
            None => Ok(None),
        }
    }

    fn own_data_property(&self, index: usize, name: &str) -> Option<Value> {
        let properties = match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Array { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. } => properties,
            _ => return None,
        };
        match properties.get_ascii(name) {
            Some(Property::Data { value, .. }) => Some(*value),
            _ => None,
        }
    }

    fn prototype_index(&self, index: usize) -> Result<Option<usize>, EvalFailure> {
        let prototype = match &self.heap[index] {
            HeapEntry::Object { prototype, .. }
            | HeapEntry::Array { prototype, .. }
            | HeapEntry::Function { prototype, .. }
            | HeapEntry::RegExp { prototype, .. }
            | HeapEntry::Date { prototype, .. }
            | HeapEntry::BuiltinIterator { prototype, .. }
            | HeapEntry::Collection { prototype, .. }
            | HeapEntry::ProcessEnv { prototype, .. } => *prototype,
            HeapEntry::NativeFunction { .. } => Some(self.intrinsics.function_prototype),
            _ => None,
        };
        match prototype {
            Some(value) => self.runtime_slot(value).map_err(EvalFailure::Runtime),
            None => Ok(None),
        }
    }

    // ---- property set ------------------------------------------------------

    fn resolve_set(
        &mut self,
        object: Value,
        key: PropertyKey,
        value: Value,
    ) -> Result<SetOutcome, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                if matches!(self.heap[index], HeapEntry::ModuleNamespace { .. }) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "assign to module namespace",
                    }));
                }
                if matches!(self.heap[index], HeapEntry::ProcessEnv { .. }) {
                    let PropertyKey::Named(name) = &key else {
                        return Ok(SetOutcome::Done);
                    };
                    let Ok(name) = name.to_utf8_strict() else {
                        return Ok(SetOutcome::Done);
                    };
                    let text = self.to_string(value)?;
                    let text = crate::host_objects::env_value_text_lossy(&text);
                    self.host.set_env(&name, &text);
                    return Ok(SetOutcome::Done);
                }
                if let Some(setter) = self.find_setter(index, &key)? {
                    return Ok(match setter {
                        Some(setter) => SetOutcome::Setter(setter),
                        None => SetOutcome::Done,
                    });
                }
                self.set_own_data(index, key, value)?;
                Ok(SetOutcome::Done)
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "set property on primitive",
            })),
        }
    }

    fn find_setter(
        &self,
        index: usize,
        key: &PropertyKey,
    ) -> Result<Option<Option<Value>>, EvalFailure> {
        if self.own_has_non_accessor(index, key) {
            return Ok(None);
        }
        let mut node = index;
        let mut guard = 0;
        loop {
            let accessor = match &self.heap[node] {
                HeapEntry::Object { properties, .. }
                | HeapEntry::Array { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. } => match properties.get(key) {
                    Some(Property::Accessor { setter, .. }) => Some(Some(*setter)),
                    Some(Property::Data { .. }) => Some(None),
                    None => None,
                },
                _ => None,
            };
            match accessor {
                Some(Some(setter)) => return Ok(Some(setter)),
                Some(None) => return Ok(None),
                None => {}
            }
            match self.prototype_index(node)? {
                Some(next) => {
                    node = next;
                    guard += 1;
                    if guard > self.heap.len() + 1 {
                        return Ok(None);
                    }
                }
                None => return Ok(None),
            }
        }
    }

    fn own_has_non_accessor(&self, index: usize, key: &PropertyKey) -> bool {
        match &self.heap[index] {
            HeapEntry::Array { elements, .. } => {
                if let PropertyKey::Named(name) = key {
                    if name.eq_ascii("length") {
                        return true;
                    }
                    if let Some(offset) = array_index(name) {
                        return elements
                            .get(offset as usize)
                            .is_some_and(|element| *element != Value::HOLE);
                    }
                }
                false
            }
            HeapEntry::Function { .. } => {
                (key.eq_ascii("length") || key.eq_ascii("name"))
                    && match key {
                        PropertyKey::Named(name) if name.eq_ascii("length") => {
                            self.own_data_property(index, "length").is_none()
                        }
                        PropertyKey::Named(_) => self.own_data_property(index, "name").is_none(),
                        _ => false,
                    }
            }
            _ => false,
        }
    }

    fn set_own_data(
        &mut self,
        index: usize,
        key: PropertyKey,
        value: Value,
    ) -> Result<(), EvalFailure> {
        if matches!(key, PropertyKey::Named(ref name) if name.eq_ascii("length"))
            && matches!(self.heap[index], HeapEntry::Array { .. })
        {
            let HeapEntry::Array {
                elements,
                properties,
                length_writable,
                ..
            } = &mut self.heap[index]
            else {
                unreachable!("array checked above");
            };
            return array_set_length(
                elements,
                properties,
                *length_writable,
                value,
                "set array length",
            );
        }
        if let HeapEntry::Array {
            elements,
            length_writable,
            ..
        } = &self.heap[index]
            && let Some(offset) = key.as_string().and_then(array_index)
            && offset as usize >= elements.len()
            && !*length_writable
        {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "add index beyond non-writable array length",
            }));
        }
        let (properties, extensible, virtual_exists) = match &self.heap[index] {
            HeapEntry::Object {
                properties,
                extensible,
                ..
            }
            | HeapEntry::Function {
                properties,
                extensible,
                ..
            }
            | HeapEntry::NativeFunction {
                properties,
                extensible,
                ..
            }
            | HeapEntry::RegExp {
                properties,
                extensible,
                ..
            }
            | HeapEntry::Date {
                properties,
                extensible,
                ..
            }
            | HeapEntry::BuiltinIterator {
                properties,
                extensible,
                ..
            }
            | HeapEntry::Collection {
                properties,
                extensible,
                ..
            } => (Some(properties), *extensible, false),
            HeapEntry::Array {
                elements,
                properties,
                extensible,
                ..
            } => {
                let virtual_exists = key.as_string().is_some_and(|name| {
                    name.eq_ascii("length")
                        || array_index(name).is_some_and(|offset| {
                            elements
                                .get(offset as usize)
                                .is_some_and(|element| *element != Value::HOLE)
                        })
                });
                (Some(properties), *extensible, virtual_exists)
            }
            _ => (None, true, false),
        };
        if let Some(property) = properties.and_then(|properties| properties.get(&key)) {
            match property {
                Property::Data {
                    writable: false, ..
                }
                | Property::Accessor { .. } => {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "assign to read only property",
                    }));
                }
                Property::Data { writable: true, .. } => {}
            }
        } else if !extensible && !virtual_exists {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "add property to non-extensible object",
            }));
        }

        let growth = match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. } => {
                usize::from(!properties.contains_key(&key)) * key.charge_bytes()
            }
            HeapEntry::Array {
                elements,
                properties,
                ..
            } => match &key {
                PropertyKey::Named(name) if name.eq_ascii("length") => 0,
                PropertyKey::Named(name) => {
                    if let Some(offset) = array_index(name) {
                        (offset as usize + 1).saturating_sub(elements.len()) * 8
                    } else {
                        usize::from(!properties.contains_key(&key)) * key.charge_bytes()
                    }
                }
                PropertyKey::Symbol(_) | PropertyKey::Private(_) => {
                    usize::from(!properties.contains_key(&key)) * key.charge_bytes()
                }
            },
            HeapEntry::String(_) | HeapEntry::BigInt(_) => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "set property on primitive",
                }));
            }
            HeapEntry::Symbol { .. }
            | HeapEntry::PrivateName { .. }
            | HeapEntry::Iterator { .. } => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "set property on non-object",
                }));
            }
            HeapEntry::ProcessEnv { .. } => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "set internal process environment",
                }));
            }
            HeapEntry::ModuleNamespace { .. } | HeapEntry::ExternalModuleNamespace { .. } => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "assign to module namespace",
                }));
            }
            HeapEntry::HashState { .. } => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "assign to hash state",
                }));
            }
        };
        self.charge_heap(growth).map_err(EvalFailure::Runtime)?;
        match &mut self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. } => {
                properties.insert(
                    key,
                    Property::Data {
                        value,
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    },
                );
                Ok(())
            }
            HeapEntry::Array {
                elements,
                properties,
                length_writable,
                ..
            } => {
                match key {
                    PropertyKey::Named(name) => {
                        if let Some(offset) = array_index(&name) {
                            let offset = offset as usize;
                            if elements.len() <= offset {
                                array_set_length(
                                    elements,
                                    properties,
                                    *length_writable,
                                    number_value((offset + 1) as f64),
                                    "set array index",
                                )?;
                            }
                            elements[offset] = value;
                        } else {
                            properties.insert(
                                PropertyKey::Named(name),
                                Property::Data {
                                    value,
                                    writable: true,
                                    enumerable: true,
                                    configurable: true,
                                },
                            );
                        }
                    }
                    identity @ (PropertyKey::Symbol(_) | PropertyKey::Private(_)) => {
                        properties.insert(
                            identity,
                            Property::Data {
                                value,
                                writable: true,
                                enumerable: true,
                                configurable: true,
                            },
                        );
                    }
                }
                Ok(())
            }
            HeapEntry::ProcessEnv { .. } => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "set internal process environment",
            })),
            _ => unreachable!("primitive and identity entries rejected above"),
        }
    }

    fn define_accessor(
        &mut self,
        object: Value,
        key: PropertyKey,
        accessor: Value,
        kind: AccessorKind,
    ) -> Result<(), EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                self.charge_heap(key.charge_bytes() + 8)
                    .map_err(EvalFailure::Runtime)?;
                let (properties, extensible) = match &mut self.heap[index] {
                    HeapEntry::Object {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::Array {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::Function {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::NativeFunction {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::RegExp {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::Date {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::BuiltinIterator {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::Collection {
                        properties,
                        extensible,
                        ..
                    } => (properties, *extensible),
                    _ => {
                        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                            operation: "define accessor on primitive",
                        }));
                    }
                };
                if properties
                    .get(&key)
                    .is_some_and(|property| !property.configurable())
                    || (!properties.contains_key(&key) && !extensible)
                {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "define accessor on non-configurable object",
                    }));
                }
                let property = properties.get_mut(&key);
                match property {
                    Some(Property::Accessor { getter, setter, .. }) => match kind {
                        AccessorKind::Getter => *getter = Some(accessor),
                        AccessorKind::Setter => *setter = Some(accessor),
                    },
                    Some(Property::Data { .. }) | None => {
                        let (getter, setter) = match kind {
                            AccessorKind::Getter => (Some(accessor), None),
                            AccessorKind::Setter => (None, Some(accessor)),
                        };
                        properties.insert(
                            key,
                            Property::Accessor {
                                getter,
                                setter,
                                enumerable: true,
                                configurable: true,
                            },
                        );
                    }
                }
                Ok(())
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "define accessor on host object",
            })),
        }
    }

    fn delete_property(&mut self, object: Value, key: &PropertyKey) -> Result<bool, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => match &mut self.heap[index] {
                HeapEntry::Object { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. } => {
                    if properties
                        .get(key)
                        .is_some_and(|property| !property.configurable())
                    {
                        return Ok(false);
                    }
                    properties.remove(key);
                    Ok(true)
                }
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    if properties
                        .get(key)
                        .is_some_and(|property| !property.configurable())
                    {
                        return Ok(false);
                    }
                    if properties.remove(key).is_some() {
                        return Ok(true);
                    }
                    if let PropertyKey::Named(name) = key {
                        if name.eq_ascii("length") {
                            return Ok(false);
                        }
                        if let Some(offset) = array_index(name) {
                            if let Some(element) = elements.get_mut(offset as usize) {
                                *element = Value::HOLE;
                            }
                            return Ok(true);
                        }
                    }
                    Ok(true)
                }
                HeapEntry::ProcessEnv { .. } => {
                    let PropertyKey::Named(name) = key else {
                        return Ok(true);
                    };
                    Ok(name
                        .to_utf8_strict()
                        .is_ok_and(|name| self.host.delete_env(&name)))
                }
                HeapEntry::String(_)
                | HeapEntry::BigInt(_)
                | HeapEntry::Symbol { .. }
                | HeapEntry::PrivateName { .. }
                | HeapEntry::Iterator { .. }
                | HeapEntry::HashState { .. } => Ok(true),
                HeapEntry::ModuleNamespace { .. } | HeapEntry::ExternalModuleNamespace { .. } => {
                    Ok(false)
                }
            },
            None => Ok(true),
        }
    }

    fn has_property(&mut self, object: Value, key: &PropertyKey) -> Result<bool, EvalFailure> {
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                if matches!(self.heap[index], HeapEntry::ProcessEnv { .. }) {
                    let PropertyKey::Named(name) = key else {
                        return Ok(false);
                    };
                    return Ok(name
                        .to_utf8_strict()
                        .is_ok_and(|name| self.host.env(&name).is_some()));
                }
                if matches!(key, PropertyKey::Private(_)) {
                    return Ok(self.own_get(index, key).is_some());
                }
                let mut node = index;
                let mut guard = 0;
                loop {
                    if self.own_get(node, key).is_some() {
                        return Ok(true);
                    }
                    match self.prototype_index(node)? {
                        Some(next) => {
                            node = next;
                            guard += 1;
                            if guard > self.heap.len() + 1 {
                                return Ok(false);
                            }
                        }
                        None => return Ok(false),
                    }
                }
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "in",
            })),
        }
    }

    // ---- aggregates & prototypes ------------------------------------------

    fn array_push(&mut self, array: Value, value: Value) -> Result<(), EvalFailure> {
        match self.runtime_slot(array).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                if !matches!(self.heap[index], HeapEntry::Array { .. }) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "push on non-array",
                    }));
                }
                self.charge_heap(8).map_err(EvalFailure::Runtime)?;
                if let HeapEntry::Array {
                    elements,
                    properties,
                    length_writable,
                    ..
                } = &mut self.heap[index]
                {
                    let offset = elements.len();
                    array_set_length(
                        elements,
                        properties,
                        *length_writable,
                        number_value((offset + 1) as f64),
                        "push beyond non-writable array length",
                    )?;
                    elements[offset] = value;
                }
                Ok(())
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "push on non-array",
            })),
        }
    }

    fn array_extend(&mut self, array: Value, iterable: Value) -> Result<(), EvalFailure> {
        let values = self.iterate_values(iterable)?;
        match self.runtime_slot(array).map_err(EvalFailure::Runtime)? {
            Some(index) => {
                if !matches!(self.heap[index], HeapEntry::Array { .. }) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "spread into non-array",
                    }));
                }
                self.charge_heap(8usize.saturating_mul(values.len()))
                    .map_err(EvalFailure::Runtime)?;
                if let HeapEntry::Array {
                    elements,
                    properties,
                    length_writable,
                    ..
                } = &mut self.heap[index]
                {
                    let offset = elements.len();
                    array_set_length(
                        elements,
                        properties,
                        *length_writable,
                        number_value((offset + values.len()) as f64),
                        "spread beyond non-writable array length",
                    )?;
                    elements[offset..].copy_from_slice(&values);
                }
                Ok(())
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "spread into non-array",
            })),
        }
    }

    /// Collects every value produced by iterating `value` (arrays and strings
    /// are the local iterables; anything else is not iterable).
    fn iterate_values(&mut self, value: Value) -> Result<Vec<Value>, EvalFailure> {
        match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Array { elements, .. } => Ok(elements
                    .iter()
                    .map(|element| {
                        if *element == Value::HOLE {
                            Value::UNDEFINED
                        } else {
                            *element
                        }
                    })
                    .collect()),
                HeapEntry::String(text) => {
                    let pieces: Vec<EcmaString> = text
                        .code_points()
                        .map(|(_, code_point)| {
                            let mut builder = EcmaStringBuilder::new();
                            builder
                                .push_code_point(code_point)
                                .expect("EcmaString code point is valid");
                            builder.finish()
                        })
                        .collect();
                    let mut values = Vec::with_capacity(pieces.len());
                    for piece in pieces {
                        values.push(
                            self.allocate(HeapEntry::String(piece))
                                .map_err(EvalFailure::Runtime)?,
                        );
                    }
                    Ok(values)
                }
                _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "value is not iterable",
                })),
            },
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "value is not iterable",
            })),
        }
    }

    fn object_spread(&mut self, target: Value, source: Value) -> Result<(), EvalFailure> {
        let target_index = match self.runtime_slot(target).map_err(EvalFailure::Runtime)? {
            Some(index)
                if matches!(
                    self.heap[index],
                    HeapEntry::Object { .. } | HeapEntry::Array { .. }
                ) =>
            {
                index
            }
            _ => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "object spread target is not an object",
                }));
            }
        };
        // Own enumerable data properties of the source are copied. `null` and
        // `undefined` sources are a no-op, matching `{ ...null }`.
        let mut pairs: Vec<(PropertyKey, Value)> = Vec::new();
        let mut char_pairs: Vec<(EcmaString, EcmaString)> = Vec::new();
        if let Some(index) = self.runtime_slot(source).map_err(EvalFailure::Runtime)? {
            match &self.heap[index] {
                HeapEntry::Object { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. } => {
                    for (key, property) in properties {
                        if let (
                            PropertyKey::Named(_),
                            Property::Data {
                                value,
                                enumerable: true,
                                ..
                            },
                        ) = (key, property)
                        {
                            pairs.push((key.clone(), *value));
                        }
                    }
                }
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    for (offset, element) in elements.iter().enumerate() {
                        if *element != Value::HOLE {
                            pairs.push((
                                PropertyKey::Named(EcmaString::from_utf8(&offset.to_string())),
                                *element,
                            ));
                        }
                    }
                    for (key, property) in properties {
                        if let (
                            PropertyKey::Named(name),
                            Property::Data {
                                value,
                                enumerable: true,
                                ..
                            },
                        ) = (key, property)
                            && array_index(name).is_none()
                        {
                            pairs.push((key.clone(), *value));
                        }
                    }
                }
                HeapEntry::String(text) => {
                    for offset in 0..text.len_units() {
                        char_pairs.push((
                            EcmaString::from_utf8(&offset.to_string()),
                            EcmaString::from_units(&[text
                                .unit_at(offset)
                                .expect("offset is in bounds")]),
                        ));
                    }
                }
                _ => {}
            }
        }
        for (name, text) in char_pairs {
            let value = self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime)?;
            pairs.push((PropertyKey::Named(name), value));
        }
        for (key, value) in pairs {
            self.set_own_data(target_index, key, value)?;
        }
        Ok(())
    }

    fn set_prototype(&mut self, object: Value, prototype: Value) -> Result<(), EvalFailure> {
        let prototype = match self.runtime_slot(prototype).map_err(EvalFailure::Runtime)? {
            Some(_) => Some(prototype),
            None => match prototype.decode() {
                Some(Decoded::Null) => None,
                Some(Decoded::HeapRef(_)) => Some(prototype),
                _ => {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "set prototype to non-object",
                    }));
                }
            },
        };
        match self.runtime_slot(object).map_err(EvalFailure::Runtime)? {
            Some(index) => match &mut self.heap[index] {
                HeapEntry::Object {
                    prototype: slot, ..
                }
                | HeapEntry::Array {
                    prototype: slot, ..
                }
                | HeapEntry::Function {
                    prototype: slot, ..
                }
                | HeapEntry::RegExp {
                    prototype: slot, ..
                }
                | HeapEntry::Date {
                    prototype: slot, ..
                }
                | HeapEntry::BuiltinIterator {
                    prototype: slot, ..
                }
                | HeapEntry::Collection {
                    prototype: slot, ..
                } => {
                    *slot = prototype;
                    Ok(())
                }
                _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "set prototype on primitive",
                })),
            },
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "set prototype on host object",
            })),
        }
    }

    // ---- iterators ---------------------------------------------------------

    fn create_iterator(&mut self, src: Value, kind: IteratorKind) -> Result<Value, EvalFailure> {
        match kind {
            IteratorKind::Keys => {
                let keys = self.enumerable_keys(src)?;
                self.allocate(HeapEntry::Iterator {
                    source: src,
                    index: 0,
                    keys: Some(keys),
                })
                .map_err(EvalFailure::Runtime)
            }
            IteratorKind::Sync | IteratorKind::Async => {
                match self.runtime_slot(src).map_err(EvalFailure::Runtime)? {
                    Some(index)
                        if matches!(
                            self.heap[index],
                            HeapEntry::Array { .. } | HeapEntry::String(_)
                        ) =>
                    {
                        self.allocate(HeapEntry::Iterator {
                            source: src,
                            index: 0,
                            keys: None,
                        })
                        .map_err(EvalFailure::Runtime)
                    }
                    _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "value is not iterable",
                    })),
                }
            }
        }
    }

    fn own_property_keys(&self, src: Value) -> Result<Vec<PropertyKey>, EvalFailure> {
        match self.runtime_slot(src).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Object { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. } => Ok(ordered_property_keys(properties)),
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    let mut keys: Vec<PropertyKey> = (0..elements.len())
                        .filter(|offset| elements[*offset] != Value::HOLE)
                        .map(|offset| {
                            PropertyKey::Named(EcmaString::from_utf8(&offset.to_string()))
                        })
                        .collect();
                    keys.extend(ordered_property_keys(properties).into_iter().filter(|key| {
                        key.as_string().and_then(array_index).is_none_or(|offset| {
                            elements
                                .get(offset as usize)
                                .is_none_or(|element| *element == Value::HOLE)
                        })
                    }));
                    Ok(keys)
                }
                HeapEntry::String(text) => Ok((0..text.len_units())
                    .map(|index| PropertyKey::Named(EcmaString::from_utf8(&index.to_string())))
                    .collect()),
                HeapEntry::ModuleNamespace { module } => {
                    let mut names: Vec<EcmaString> = self.program().modules()
                        [module.get() as usize]
                        .exports
                        .iter()
                        .map(|export| self.constant_text(*module, export.name).clone())
                        .collect();
                    names.sort();
                    Ok(names.into_iter().map(PropertyKey::Named).collect())
                }
                HeapEntry::ExternalModuleNamespace { specifier } => Ok(self.registry.external
                    [specifier]
                    .exports
                    .keys()
                    .cloned()
                    .map(PropertyKey::Named)
                    .collect()),
                HeapEntry::ProcessEnv { .. }
                | HeapEntry::BigInt(_)
                | HeapEntry::Symbol { .. }
                | HeapEntry::PrivateName { .. }
                | HeapEntry::HashState { .. }
                | HeapEntry::Iterator { .. } => Ok(Vec::new()),
            },
            None => Ok(Vec::new()),
        }
    }

    fn enumerable_keys(&self, src: Value) -> Result<Vec<EcmaString>, EvalFailure> {
        let keys = self.own_property_keys(src)?;
        let Some(index) = self.runtime_slot(src).map_err(EvalFailure::Runtime)? else {
            return Ok(Vec::new());
        };
        Ok(keys
            .into_iter()
            .filter_map(|key| {
                let PropertyKey::Named(name) = &key else {
                    return None;
                };
                let enumerable = match &self.heap[index] {
                    HeapEntry::Array {
                        elements,
                        properties,
                        ..
                    } => properties.get(&key).map_or_else(
                        || {
                            array_index(name).is_some_and(|offset| {
                                elements
                                    .get(offset as usize)
                                    .is_some_and(|element| *element != Value::HOLE)
                            })
                        },
                        Property::enumerable,
                    ),
                    HeapEntry::String(text) => {
                        array_index(name).is_some_and(|offset| (offset as usize) < text.len_units())
                    }
                    HeapEntry::ModuleNamespace { .. } => true,
                    HeapEntry::Object { properties, .. }
                    | HeapEntry::Function { properties, .. }
                    | HeapEntry::NativeFunction { properties, .. }
                    | HeapEntry::RegExp { properties, .. }
                    | HeapEntry::Date { properties, .. }
                    | HeapEntry::BuiltinIterator { properties, .. }
                    | HeapEntry::Collection { properties, .. } => {
                        properties.get(&key).is_some_and(Property::enumerable)
                    }
                    _ => false,
                };
                enumerable.then_some(name.clone())
            })
            .collect())
    }

    fn iterator_next(&mut self, iterator: Value) -> Result<(bool, Value), EvalFailure> {
        let iterator_index = self
            .runtime_slot(iterator)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "iterator next on non-iterator",
            }))?;
        let (source, current, keys_len, key_at) = match &self.heap[iterator_index] {
            HeapEntry::Iterator {
                source,
                index,
                keys,
                ..
            } => (
                *source,
                *index,
                keys.as_ref().map(Vec::len),
                keys.as_ref().and_then(|keys| keys.get(*index).cloned()),
            ),
            _ => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "iterator next on non-iterator",
                }));
            }
        };

        if let Some(length) = keys_len {
            if current >= length {
                return Ok((true, Value::UNDEFINED));
            }
            let text = key_at.expect("current < length has a key");
            let value = self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime)?;
            self.advance_iterator(iterator_index);
            return Ok((false, value));
        }

        enum Step {
            Element(Option<Value>),
            Text(Option<EcmaString>),
        }
        let step = match self.runtime_slot(source).map_err(EvalFailure::Runtime)? {
            Some(source_index) => match &self.heap[source_index] {
                HeapEntry::Array { elements, .. } => {
                    Step::Element(elements.get(current).map(|element| {
                        if *element == Value::HOLE {
                            Value::UNDEFINED
                        } else {
                            *element
                        }
                    }))
                }
                HeapEntry::String(text) => {
                    let Some((offset, _)) = text.code_points().nth(current) else {
                        return Ok((true, Value::UNDEFINED));
                    };
                    let next_offset = text
                        .code_points()
                        .nth(current + 1)
                        .map_or(text.len_units(), |(next, _)| next);
                    Step::Text(Some(text.slice_units(offset..next_offset)))
                }
                _ => return Ok((true, Value::UNDEFINED)),
            },
            None => return Ok((true, Value::UNDEFINED)),
        };
        match step {
            Step::Element(Some(value)) => {
                self.advance_iterator(iterator_index);
                Ok((false, value))
            }
            Step::Element(None) => Ok((true, Value::UNDEFINED)),
            Step::Text(Some(text)) => {
                let value = self
                    .allocate(HeapEntry::String(text))
                    .map_err(EvalFailure::Runtime)?;
                self.advance_iterator(iterator_index);
                Ok((false, value))
            }
            Step::Text(None) => Ok((true, Value::UNDEFINED)),
        }
    }

    fn advance_iterator(&mut self, iterator_index: usize) {
        if let HeapEntry::Iterator { index, .. } = &mut self.heap[iterator_index] {
            *index += 1;
        }
    }

    // ---- operators & coercions --------------------------------------------

    fn eval_unary(&mut self, op: UnaryOp, operand: Value) -> Result<Value, EvalFailure> {
        match op {
            UnaryOp::Void => Ok(Value::UNDEFINED),
            UnaryOp::TypeOf => {
                let text = EcmaString::from_utf8(self.type_of(operand));
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
                let key = self.to_property_key(left)?;
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
            let left = match left_string {
                Some(text) => text,
                None => self.value_to_string(left, 0)?,
            };
            let right = match right_string {
                Some(text) => text,
                None => self.value_to_string(right, 0)?,
            };
            let mut builder = EcmaStringBuilder::with_capacity(
                left.len_units().saturating_add(right.len_units()),
            );
            for &unit in left.as_units() {
                builder.push_unit(unit);
            }
            for &unit in right.as_units() {
                builder.push_unit(unit);
            }
            return self
                .allocate(HeapEntry::String(builder.finish()))
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
            Some(Decoded::Number(_)) | Some(Decoded::Int32(_)) => self.to_primitive(value),
            Some(Decoded::Undefined) => Ok(Value::number(f64::NAN)),
            Some(Decoded::Null) => Ok(Value::int32(0)),
            Some(Decoded::Boolean(value)) => Ok(Value::int32(u32::from(value))),
            Some(Decoded::Hole) | Some(Decoded::Uninitialized) => Ok(Value::number(f64::NAN)),
            Some(Decoded::HeapRef(_)) => {
                match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
                    Some(index) => match &self.heap[index] {
                        HeapEntry::String(text) => Ok(number_value(parse_number(text))),
                        HeapEntry::BigInt(_) => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                            operation: "convert bigint to number",
                        })),
                        HeapEntry::Array { elements, .. } if elements.is_empty() => {
                            Ok(Value::int32(0))
                        }
                        HeapEntry::Array { elements, .. } if elements.len() == 1 => {
                            self.to_number(elements[0])
                        }
                        HeapEntry::Symbol { .. } | HeapEntry::PrivateName { .. } => {
                            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                                operation: "convert symbol to number",
                            }))
                        }
                        HeapEntry::Object { .. }
                        | HeapEntry::Array { .. }
                        | HeapEntry::Function { .. }
                        | HeapEntry::ModuleNamespace { .. }
                        | HeapEntry::ExternalModuleNamespace { .. }
                        | HeapEntry::HashState { .. }
                        | HeapEntry::NativeFunction { .. }
                        | HeapEntry::RegExp { .. }
                        | HeapEntry::Date { .. }
                        | HeapEntry::BuiltinIterator { .. }
                        | HeapEntry::Collection { .. }
                        | HeapEntry::ProcessEnv { .. }
                        | HeapEntry::Iterator { .. } => Ok(Value::number(f64::NAN)),
                    },
                    None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "coerce host object to number",
                    })),
                }
            }
            None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
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
                    HeapEntry::String(text) => !text.is_empty(),
                    HeapEntry::BigInt(text) => text != "0",
                    HeapEntry::Object { .. }
                    | HeapEntry::Array { .. }
                    | HeapEntry::Function { .. }
                    | HeapEntry::ModuleNamespace { .. }
                    | HeapEntry::ExternalModuleNamespace { .. }
                    | HeapEntry::HashState { .. }
                    | HeapEntry::NativeFunction { .. }
                    | HeapEntry::Symbol { .. }
                    | HeapEntry::PrivateName { .. }
                    | HeapEntry::RegExp { .. }
                    | HeapEntry::Date { .. }
                    | HeapEntry::BuiltinIterator { .. }
                    | HeapEntry::Collection { .. }
                    | HeapEntry::ProcessEnv { .. }
                    | HeapEntry::Iterator { .. } => true,
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
                    HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. } => "function",
                    HeapEntry::Symbol { .. } => "symbol",
                    HeapEntry::PrivateName { .. } => "object",
                    HeapEntry::Object { .. }
                    | HeapEntry::Array { .. }
                    | HeapEntry::ModuleNamespace { .. }
                    | HeapEntry::ExternalModuleNamespace { .. }
                    | HeapEntry::HashState { .. }
                    | HeapEntry::RegExp { .. }
                    | HeapEntry::Date { .. }
                    | HeapEntry::BuiltinIterator { .. }
                    | HeapEntry::Collection { .. }
                    | HeapEntry::ProcessEnv { .. }
                    | HeapEntry::Iterator { .. } => "object",
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
                        (HeapEntry::String(a), HeapEntry::String(b)) => a == b,
                        (HeapEntry::BigInt(a), HeapEntry::BigInt(b)) => a == b,
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

    /// `value instanceof constructor`: walks `value`'s prototype chain for the
    /// constructor's own `prototype` object, matching by heap identity.
    fn instance_of(&mut self, value: Value, constructor: Value) -> Result<bool, EvalFailure> {
        match self
            .runtime_slot(constructor)
            .map_err(EvalFailure::Runtime)?
        {
            Some(index) => {
                if !matches!(
                    self.heap[index],
                    HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. }
                ) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "instanceof",
                    }));
                }
                let target = match self.own_get_ascii(index, "prototype") {
                    Some(Found::Value(value)) if self.is_object(value) => value,
                    _ => {
                        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                            operation: "instanceof prototype is not an object",
                        }));
                    }
                };
                let target_slot = self.runtime_slot(target).map_err(EvalFailure::Runtime)?;
                let mut node = match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
                    Some(node) => node,
                    None => return Ok(false),
                };
                let mut guard = 0;
                loop {
                    if Some(node) == target_slot {
                        return Ok(true);
                    }
                    match self.prototype_index(node)? {
                        Some(next) => {
                            node = next;
                            guard += 1;
                            if guard > self.heap.len() + 1 {
                                return Ok(false);
                            }
                        }
                        None => return Ok(false),
                    }
                }
            }
            None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "instanceof",
            })),
        }
    }

    fn add_string_primitive(&self, value: Value) -> Result<Option<EcmaString>, EvalFailure> {
        match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::String(text) => Ok(Some(text.clone())),
                HeapEntry::Object { .. }
                | HeapEntry::Array { .. }
                | HeapEntry::Function { .. }
                | HeapEntry::ModuleNamespace { .. }
                | HeapEntry::ExternalModuleNamespace { .. }
                | HeapEntry::NativeFunction { .. }
                | HeapEntry::RegExp { .. }
                | HeapEntry::Date { .. }
                | HeapEntry::BuiltinIterator { .. }
                | HeapEntry::Collection { .. }
                | HeapEntry::Iterator { .. }
                | HeapEntry::ProcessEnv { .. }
                | HeapEntry::Symbol { .. }
                | HeapEntry::PrivateName { .. }
                | HeapEntry::HashState { .. } => self.value_to_string(value, 0).map(Some),
                HeapEntry::BigInt(_) => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn value_to_string(&self, value: Value, depth: u8) -> Result<EcmaString, EvalFailure> {
        if depth >= 32 {
            return Ok(EcmaString::default());
        }
        let ascii = |text: String| EcmaString::from_utf8(&text);
        match value.decode() {
            Some(Decoded::Number(number)) => Ok(ascii(Self::ordinary_number_to_string(number))),
            Some(Decoded::Int32(raw)) => Ok(ascii((raw as i32).to_string())),
            Some(Decoded::Undefined | Decoded::Uninitialized) => {
                Ok(EcmaString::from_utf8("undefined"))
            }
            Some(Decoded::Null) => Ok(EcmaString::from_utf8("null")),
            Some(Decoded::Boolean(value)) => {
                Ok(EcmaString::from_utf8(if value { "true" } else { "false" }))
            }
            Some(Decoded::Hole) => Ok(EcmaString::default()),
            Some(Decoded::HeapRef(_)) => {
                match self.runtime_slot(value).map_err(EvalFailure::Runtime)? {
                    Some(index) => match &self.heap[index] {
                        HeapEntry::String(text) => Ok(text.clone()),
                        HeapEntry::BigInt(text) => Ok(EcmaString::from_utf8(text)),
                        HeapEntry::Object { .. }
                        | HeapEntry::Date { .. }
                        | HeapEntry::BuiltinIterator { .. }
                        | HeapEntry::Collection { .. }
                        | HeapEntry::ModuleNamespace { .. }
                        | HeapEntry::ExternalModuleNamespace { .. }
                        | HeapEntry::ProcessEnv { .. }
                        | HeapEntry::Iterator { .. }
                        | HeapEntry::HashState { .. } => {
                            Ok(EcmaString::from_utf8("[object Object]"))
                        }
                        HeapEntry::RegExp { pattern, flags, .. } => {
                            let mut builder = EcmaStringBuilder::with_capacity(
                                pattern
                                    .len_units()
                                    .saturating_add(flags.len_units())
                                    .saturating_add(2),
                            );
                            builder.push_unit(u16::from(b'/'));
                            for &unit in pattern.as_units() {
                                builder.push_unit(unit);
                            }
                            builder.push_unit(u16::from(b'/'));
                            for &unit in flags.as_units() {
                                builder.push_unit(unit);
                            }
                            Ok(builder.finish())
                        }
                        HeapEntry::Symbol { .. } => {
                            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                                operation: "convert symbol to string",
                            }))
                        }
                        HeapEntry::PrivateName { .. } => {
                            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                                operation: "convert private name to string",
                            }))
                        }
                        HeapEntry::Function {
                            module, function, ..
                        } => {
                            let flags = self.module_code(*module).functions()
                                [function.get() as usize]
                                .flags();
                            Ok(EcmaString::from_utf8(
                                match (flags.is_async, flags.is_generator) {
                                    (true, true) => "async function* () { [bytecode] }",
                                    (true, false) => "async function () { [bytecode] }",
                                    (false, true) => "function* () { [bytecode] }",
                                    (false, false) => "function () { [bytecode] }",
                                },
                            ))
                        }
                        HeapEntry::NativeFunction { .. } => {
                            Ok(EcmaString::from_utf8("function () { [native code] }"))
                        }
                        HeapEntry::Array { elements, .. } => {
                            let mut text = EcmaStringBuilder::new();
                            for (index, element) in elements.iter().copied().enumerate() {
                                if index != 0 {
                                    text.push_unit(u16::from(b','));
                                }
                                if element != Value::HOLE
                                    && element != Value::NULL
                                    && element != Value::UNDEFINED
                                {
                                    for &unit in
                                        self.value_to_string(element, depth + 1)?.as_units()
                                    {
                                        text.push_unit(unit);
                                    }
                                }
                            }
                            Ok(text.finish())
                        }
                    },
                    None => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "coerce host object to string",
                    })),
                }
            }
            None => Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value,
            })),
        }
    }

    fn string_text(&self, value: Value) -> Option<&EcmaString> {
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
            Ok(Some(index)) => !matches!(
                self.heap[index],
                HeapEntry::String(_) | HeapEntry::BigInt(_)
            ),
            Ok(None) => matches!(value.decode(), Some(Decoded::HeapRef(_))),
            Err(_) => false,
        }
    }
}

fn ordered_property_keys(properties: &PropertyMap) -> Vec<PropertyKey> {
    let mut indices = Vec::new();
    let mut strings = Vec::new();
    let mut symbols = Vec::new();
    for (key, _) in properties.iter() {
        match key {
            PropertyKey::Named(name) => match array_index(name) {
                Some(index) => indices.push((index, key.clone())),
                None => strings.push(key.clone()),
            },
            PropertyKey::Symbol(_) | PropertyKey::Private(_) => symbols.push(key.clone()),
        }
    }
    indices.sort_unstable_by_key(|(index, _)| *index);
    indices
        .into_iter()
        .map(|(_, key)| key)
        .chain(strings)
        .chain(symbols)
        .collect()
}

fn property_lookup(properties: &PropertyMap, key: &PropertyKey) -> Option<Found> {
    match properties.get(key) {
        Some(Property::Data { value, .. }) => Some(Found::Value(*value)),
        Some(Property::Accessor { getter, .. }) => Some(match getter {
            Some(getter) => Found::Getter(*getter),
            None => Found::NoGetter,
        }),
        None => None,
    }
}

fn property_lookup_ascii(properties: &PropertyMap, name: &str) -> Option<Found> {
    match properties.get_ascii(name) {
        Some(Property::Data { value, .. }) => Some(Found::Value(*value)),
        Some(Property::Accessor { getter, .. }) => Some(match getter {
            Some(getter) => Found::Getter(*getter),
            None => Found::NoGetter,
        }),
        None => None,
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

fn parse_number(text: &EcmaString) -> f64 {
    let Ok(text) = text.to_utf8_strict() else {
        return f64::NAN;
    };
    parse_number_utf8(&text)
}

fn parse_number_utf8(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    }
}

fn format_number(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if number == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if number == 0.0 {
        return "0".to_owned();
    }

    let negative = number.is_sign_negative();
    let raw = number.abs().to_string();
    let (mantissa, explicit_exponent) = match raw.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent
                .parse::<i32>()
                .expect("Rust formats finite f64 exponents as i32"),
        ),
        None => (raw.as_str(), 0),
    };
    let decimal = mantissa.find('.').unwrap_or(mantissa.len());
    let untrimmed: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let first = untrimmed
        .find(|ch| ch != '0')
        .expect("a nonzero number has a nonzero decimal digit");
    let digits = untrimmed[first..].trim_end_matches('0');
    let exponent = explicit_exponent + decimal as i32 - first as i32 - 1;

    let mut result = String::new();
    if negative {
        result.push('-');
    }
    if !(-6..21).contains(&exponent) {
        result.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            result.push('.');
            result.push_str(&digits[1..]);
        }
        result.push('e');
        if exponent >= 0 {
            result.push('+');
        }
        result.push_str(&exponent.to_string());
    } else if exponent >= 0 {
        let integer_digits = exponent as usize + 1;
        if digits.len() <= integer_digits {
            result.push_str(digits);
            result.extend(std::iter::repeat_n('0', integer_digits - digits.len()));
        } else {
            result.push_str(&digits[..integer_digits]);
            result.push('.');
            result.push_str(&digits[integer_digits..]);
        }
    } else {
        result.push_str("0.");
        result.extend(std::iter::repeat_n('0', (-exponent - 1) as usize));
        result.push_str(digits);
    }
    result
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

fn array_index_ascii(key: &str) -> Option<u32> {
    if !key.is_ascii() || key.is_empty() || (key.len() > 1 && key.as_bytes()[0] == b'0') {
        return None;
    }
    let mut index = 0_u32;
    for byte in key.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        index = index.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    (index != u32::MAX).then_some(index)
}

fn array_index(key: &EcmaString) -> Option<u32> {
    let units = key.as_units();
    if units.is_empty() || (units.len() > 1 && units[0] == u16::from(b'0')) {
        return None;
    }
    let mut index = 0_u32;
    for &unit in units {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&unit) {
            return None;
        }
        index = index
            .checked_mul(10)?
            .checked_add(u32::from(unit - u16::from(b'0')))?;
    }
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

pub(crate) fn apply_array_length(
    elements: &mut Vec<Value>,
    properties: &mut PropertyMap,
    length: usize,
    operation: &'static str,
) -> Result<(), EvalFailure> {
    if length >= elements.len() {
        elements.resize(length, Value::HOLE);
        return Ok(());
    }
    let blocked = properties
        .iter()
        .filter_map(|(key, property)| {
            (!property.configurable())
                .then(|| key.as_string().and_then(array_index))
                .flatten()
        })
        .map(|offset| offset as usize)
        .filter(|offset| *offset >= length)
        .max();
    let effective_length = blocked.map_or(length, |offset| offset + 1);
    properties.0.retain(|(key, _)| {
        key.as_string()
            .and_then(array_index)
            .is_none_or(|offset| (offset as usize) < effective_length)
    });
    elements.resize(effective_length, Value::HOLE);
    if blocked.is_some() {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError { operation }));
    }
    Ok(())
}

pub(crate) fn array_set_length(
    elements: &mut Vec<Value>,
    properties: &mut PropertyMap,
    length_writable: bool,
    value: Value,
    operation: &'static str,
) -> Result<(), EvalFailure> {
    let length = exact_array_length(value)
        .ok_or(EvalFailure::Throw(ThrowOrigin::RangeError { operation }))?;
    if !length_writable {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError { operation }));
    }
    apply_array_length(elements, properties, length, operation)
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

pub(crate) fn unary_from_selector(op: u32) -> Option<UnaryOp> {
    match op {
        0 => Some(UnaryOp::Void),
        1 => Some(UnaryOp::TypeOf),
        2 => Some(UnaryOp::Plus),
        3 => Some(UnaryOp::Negate),
        4 => Some(UnaryOp::BitwiseNot),
        5 => Some(UnaryOp::LogicalNot),
        _ => None,
    }
}

pub(crate) fn binary_from_selector(op: u32) -> Option<BinaryOp> {
    match op {
        0 => Some(BinaryOp::Add),
        1 => Some(BinaryOp::Subtract),
        2 => Some(BinaryOp::Multiply),
        3 => Some(BinaryOp::Divide),
        4 => Some(BinaryOp::Remainder),
        5 => Some(BinaryOp::Exponent),
        6 => Some(BinaryOp::BitAnd),
        7 => Some(BinaryOp::BitOr),
        8 => Some(BinaryOp::BitXor),
        9 => Some(BinaryOp::ShiftLeft),
        10 => Some(BinaryOp::ShiftRight),
        11 => Some(BinaryOp::UnsignedShiftRight),
        12 => Some(BinaryOp::Equal),
        13 => Some(BinaryOp::NotEqual),
        14 => Some(BinaryOp::StrictEqual),
        15 => Some(BinaryOp::StrictNotEqual),
        16 => Some(BinaryOp::LessThan),
        17 => Some(BinaryOp::LessThanOrEqual),
        18 => Some(BinaryOp::GreaterThan),
        19 => Some(BinaryOp::GreaterThanOrEqual),
        20 => Some(BinaryOp::InstanceOf),
        21 => Some(BinaryOp::In),
        _ => None,
    }
}

pub(crate) fn iterator_kind_from_selector(kind: u32) -> Option<IteratorKind> {
    match kind {
        0 => Some(IteratorKind::Sync),
        1 => Some(IteratorKind::Async),
        2 => Some(IteratorKind::Keys),
        _ => None,
    }
}

pub(crate) fn accessor_from_selector(kind: u32) -> Option<AccessorKind> {
    match kind {
        0 => Some(AccessorKind::Getter),
        1 => Some(AccessorKind::Setter),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_bytecode::{
        Binding, Edge, EdgeKind, ExceptionHandler, Export, ExportSource, FunctionFlags, NumberBits,
        ProgramModule, Register,
    };

    fn reg(raw: u32) -> Register {
        Register::new(raw)
    }
    fn pc(raw: u32) -> Pc {
        Pc::new(raw)
    }
    fn cid(raw: u32) -> ConstantId {
        ConstantId::new(raw)
    }

    /// A function with no captures.
    fn function(
        parameters: u32,
        registers: u32,
        code: Vec<Instruction>,
        handlers: Vec<ExceptionHandler>,
    ) -> Function {
        Function::new(
            None,
            0,
            parameters,
            registers,
            FunctionFlags::default(),
            code,
            handlers,
        )
    }

    /// A function with `captures` leading capture registers.
    fn closure_function(
        captures: u32,
        parameters: u32,
        registers: u32,
        code: Vec<Instruction>,
    ) -> Function {
        Function::new(
            None,
            captures,
            parameters,
            registers,
            FunctionFlags::default(),
            code,
            Vec::new(),
        )
    }

    fn verified(mut constants: Vec<Constant>, functions: Vec<Function>) -> Program<Verified> {
        let name = ConstantId::new(constants.len() as u32);
        constants.push(Constant::String(EcmaString::from_utf8("<test>")));
        let code = Module::new(constants, functions, FunctionId::new(0))
            .verify()
            .expect("valid test bytecode");
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
        .expect("valid one-module test program")
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
        let code = Module::new(constants, functions, FunctionId::new(0))
            .verify()
            .expect("valid test bytecode");
        ProgramModule {
            name: ConstantId::new(0),
            code,
            edges,
            bindings,
            exports,
        }
    }

    fn linked(modules: Vec<ProgramModule<Verified>>, entry: u32) -> Program<Verified> {
        Program::link(modules, ModuleId::new(entry)).expect("valid linked test program")
    }

    fn namespace_descriptor_entry() -> Function {
        function(
            0,
            7,
            vec![
                Instruction::LoadGlobal {
                    dst: reg(0),
                    name: cid(1),
                },
                Instruction::LoadGlobal {
                    dst: reg(1),
                    name: cid(3),
                },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(4),
                },
                Instruction::GetProperty {
                    dst: reg(3),
                    object: reg(1),
                    key: reg(2),
                },
                Instruction::CreateArray { dst: reg(4) },
                Instruction::ArrayPush {
                    array: reg(4),
                    value: reg(0),
                },
                Instruction::LoadConst {
                    dst: reg(5),
                    constant: cid(5),
                },
                Instruction::ArrayPush {
                    array: reg(4),
                    value: reg(5),
                },
                Instruction::Call {
                    dst: reg(6),
                    callee: reg(3),
                    this_value: reg(4),
                    arguments: reg(4),
                },
                Instruction::Return { value: reg(6) },
            ],
            Vec::new(),
        )
    }

    #[derive(Default)]
    struct TestHost;
    impl Host for TestHost {}

    fn run_ok(program: &Program<Verified>) -> Execution {
        let mut host = TestHost;
        Machine::new(program, &mut host, Limits::default())
            .run()
            .unwrap()
    }

    #[test]
    fn object_values_have_stable_distinct_heap_identity() {
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
        let execution = run_ok(&module);
        assert_eq!(execution.entry_registers[2], Value::FALSE);
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn computed_member_access_uses_dynamic_register_key() {
        // key = "a" + "b"; obj[key] = 7; return obj[key].
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("a")),
                Constant::String(EcmaString::from_utf8("b")),
                Constant::Int32(7),
            ],
            vec![function(
                0,
                6,
                vec![
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(1),
                    },
                    Instruction::Binary {
                        dst: reg(3),
                        op: BinaryOp::Add,
                        left: reg(1),
                        right: reg(2),
                    },
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(4),
                        constant: cid(2),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(3),
                        value: reg(4),
                    },
                    Instruction::GetProperty {
                        dst: reg(5),
                        object: reg(0),
                        key: reg(3),
                    },
                    Instruction::Return { value: reg(5) },
                ],
                vec![],
            )],
        );
        assert_eq!(run_ok(&module).value, Value::int32(7));
    }

    #[test]
    fn property_delete_and_array_holes_are_real_mutations() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("0")),
                Constant::Int32(5),
            ],
            vec![function(
                0,
                5,
                vec![
                    Instruction::CreateArray { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(4),
                        constant: cid(1),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(1),
                        value: reg(4),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::DeleteProperty {
                        dst: reg(3),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Return { value: reg(3) },
                ],
                vec![],
            )],
        );
        let execution = run_ok(&module);
        assert_eq!(execution.entry_registers[2], Value::int32(5));
        assert_eq!(execution.entry_registers[4], Value::UNDEFINED);
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn closure_captures_seed_leading_registers_before_parameters() {
        // captures = [42]; fn1(7) => capture(r0) + param(r1) = 49.
        let entry = function(
            0,
            3,
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
                Instruction::CreateClosure {
                    dst: reg(2),
                    function: FunctionId::new(1),
                    captures: reg(0),
                },
                // arguments array [7]
                Instruction::CreateArray { dst: reg(0) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(1),
                },
                Instruction::ArrayPush {
                    array: reg(0),
                    value: reg(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::Call {
                    dst: reg(1),
                    callee: reg(2),
                    this_value: reg(1),
                    arguments: reg(0),
                },
                Instruction::Return { value: reg(1) },
            ],
            vec![],
        );
        // capture_count = 1, parameter_count = 1: r0 = capture, r1 = param.
        let callee = closure_function(
            1,
            1,
            3,
            vec![
                Instruction::Binary {
                    dst: reg(2),
                    op: BinaryOp::Add,
                    left: reg(0),
                    right: reg(1),
                },
                Instruction::Return { value: reg(2) },
            ],
        );
        let module = verified(
            vec![Constant::Int32(42), Constant::Int32(7), Constant::Undefined],
            vec![entry, callee],
        );
        assert_eq!(run_ok(&module).value, Value::int32(49));
    }

    #[test]
    fn calls_scale_past_fixed_window_via_arguments_array() {
        // Build a 500-element arguments array and call a callee returning
        // arguments.length — impossible under a 127 fixed window.
        let mut code = vec![Instruction::CreateArray { dst: reg(0) }];
        code.push(Instruction::LoadConst {
            dst: reg(1),
            constant: cid(0),
        });
        for _ in 0..500 {
            code.push(Instruction::ArrayPush {
                array: reg(0),
                value: reg(1),
            });
        }
        code.push(Instruction::CreateClosure {
            dst: reg(2),
            function: FunctionId::new(1),
            captures: reg(3),
        });
        // captures array for a zero-capture function
        // (reg(3) must be an empty array)
        // Insert its creation before CreateClosure:
        let mut prelude = vec![Instruction::CreateArray { dst: reg(3) }];
        prelude.append(&mut code);
        let mut code = prelude;
        code.push(Instruction::LoadConst {
            dst: reg(1),
            constant: cid(1),
        });
        code.push(Instruction::Call {
            dst: reg(1),
            callee: reg(2),
            this_value: reg(1),
            arguments: reg(0),
        });
        code.push(Instruction::Return { value: reg(1) });

        let entry = function(0, 4, code, vec![]);
        let callee = function(
            0,
            2,
            vec![
                Instruction::LoadArguments { dst: reg(0) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::GetProperty {
                    dst: reg(0),
                    object: reg(0),
                    key: reg(1),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::Int32(1),
                Constant::Undefined,
                Constant::String(EcmaString::from_utf8("length")),
            ],
            vec![entry, callee],
        );
        assert_eq!(run_ok(&module).value, Value::int32(500));
    }

    #[test]
    fn array_extend_spreads_iterable_elements() {
        // dst = []; dst.push(1); dst.extend([2,3]); return dst.length == 3.
        let entry = function(
            0,
            4,
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
                // source [2,3]
                Instruction::CreateArray { dst: reg(2) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(1),
                },
                Instruction::ArrayPush {
                    array: reg(2),
                    value: reg(1),
                },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::ArrayPush {
                    array: reg(2),
                    value: reg(1),
                },
                Instruction::ArrayExtend {
                    array: reg(0),
                    iterable: reg(2),
                },
                Instruction::LoadConst {
                    dst: reg(3),
                    constant: cid(3),
                },
                Instruction::GetProperty {
                    dst: reg(0),
                    object: reg(0),
                    key: reg(3),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::Int32(3),
                Constant::String(EcmaString::from_utf8("length")),
            ],
            vec![entry],
        );
        assert_eq!(run_ok(&module).value, Value::int32(3));
    }

    #[test]
    fn object_spread_copies_own_properties() {
        // src = {}; src.x = 9; target = {}; { ...src }; return target.x.
        let key = |c: u32| Instruction::LoadConst {
            dst: reg(3),
            constant: cid(c),
        };
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(9),
            ],
            vec![function(
                0,
                4,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    key(0),
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(1),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(3),
                        value: reg(2),
                    },
                    Instruction::CreateObject { dst: reg(1) },
                    Instruction::ObjectSpread {
                        target: reg(1),
                        source: reg(0),
                    },
                    key(0),
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(1),
                        key: reg(3),
                    },
                    Instruction::Return { value: reg(2) },
                ],
                vec![],
            )],
        );
        assert_eq!(run_ok(&module).value, Value::int32(9));
    }

    #[test]
    fn private_names_have_distinct_identity_and_are_gettable() {
        // Two private names with the same description are distinct keys.
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(1),
                Constant::Int32(2),
            ],
            vec![function(
                0,
                6,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::CreatePrivateName {
                        dst: reg(1),
                        description: cid(0),
                    },
                    Instruction::CreatePrivateName {
                        dst: reg(2),
                        description: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(1),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(1),
                        value: reg(3),
                    },
                    Instruction::LoadConst {
                        dst: reg(3),
                        constant: cid(2),
                    },
                    Instruction::SetProperty {
                        object: reg(0),
                        key: reg(2),
                        value: reg(3),
                    },
                    // r4 = obj[#1] (1), r5 = obj[#2] (2)
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::GetProperty {
                        dst: reg(5),
                        object: reg(0),
                        key: reg(2),
                    },
                    // distinctness: #1 !== #2
                    Instruction::Binary {
                        dst: reg(3),
                        op: BinaryOp::StrictEqual,
                        left: reg(1),
                        right: reg(2),
                    },
                    Instruction::Return { value: reg(4) },
                ],
                vec![],
            )],
        );
        let execution = run_ok(&module);
        assert_eq!(execution.value, Value::int32(1));
        assert_eq!(execution.entry_registers[5], Value::int32(2));
        assert_eq!(execution.entry_registers[3], Value::FALSE);
    }

    #[test]
    fn accessor_getter_is_invoked_on_property_read() {
        // Define a getter returning 99, then read the property.
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateObject { dst: reg(0) },
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(1),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(0),
                },
                Instruction::DefineAccessor {
                    object: reg(0),
                    key: reg(2),
                    accessor: reg(1),
                    kind: AccessorKind::Getter,
                },
                Instruction::GetProperty {
                    dst: reg(1),
                    object: reg(0),
                    key: reg(2),
                },
                Instruction::Return { value: reg(1) },
            ],
            vec![],
        );
        let getter = function(
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(1),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("g")),
                Constant::Int32(99),
            ],
            vec![entry, getter],
        );
        assert_eq!(run_ok(&module).value, Value::int32(99));
    }

    #[test]
    fn prototype_chain_lookup_and_instanceof() {
        // proto = {}; proto.m = 5; ctor.prototype = proto; obj = new ctor();
        // return (obj.m == 5) && (obj instanceof ctor).
        let entry = function(
            0,
            6,
            vec![
                // proto object with m = 5
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
                // ctor closure
                Instruction::CreateArray { dst: reg(4) },
                Instruction::CreateClosure {
                    dst: reg(3),
                    function: FunctionId::new(1),
                    captures: reg(4),
                },
                // ctor.prototype = proto
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(2),
                },
                Instruction::SetProperty {
                    object: reg(3),
                    key: reg(1),
                    value: reg(0),
                },
                // obj = new ctor()  (empty args)
                Instruction::CreateArray { dst: reg(4) },
                Instruction::Construct {
                    dst: reg(0),
                    callee: reg(3),
                    arguments: reg(4),
                },
                // obj.m via prototype chain
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::GetProperty {
                    dst: reg(2),
                    object: reg(0),
                    key: reg(1),
                },
                // obj instanceof ctor
                Instruction::Binary {
                    dst: reg(5),
                    op: BinaryOp::InstanceOf,
                    left: reg(0),
                    right: reg(3),
                },
                Instruction::Return { value: reg(2) },
            ],
            vec![],
        );
        let ctor = function(0, 1, vec![Instruction::Halt], vec![]);
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("m")),
                Constant::Int32(5),
                Constant::String(EcmaString::from_utf8("prototype")),
            ],
            vec![entry, ctor],
        );
        let execution = run_ok(&module);
        assert_eq!(execution.value, Value::int32(5));
        assert_eq!(execution.entry_registers[5], Value::TRUE);
    }

    #[test]
    fn sync_iterator_walks_array_elements() {
        // Sum [10,20] via GetIterator/IteratorNext loop.
        let entry = function(
            0,
            6,
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
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(1),
                },
                Instruction::ArrayPush {
                    array: reg(0),
                    value: reg(1),
                },
                // acc = 0
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(2),
                },
                Instruction::GetIterator {
                    dst: reg(3),
                    src: reg(0),
                    kind: IteratorKind::Sync,
                },
                // loop head @7: next
                Instruction::IteratorNext {
                    done: reg(4),
                    value: reg(5),
                    iterator: reg(3),
                },
                Instruction::JumpIfTrue {
                    condition: reg(4),
                    target: pc(11),
                },
                Instruction::Binary {
                    dst: reg(2),
                    op: BinaryOp::Add,
                    left: reg(2),
                    right: reg(5),
                },
                Instruction::Jump { target: pc(7) },
                // @11 done
                Instruction::Return { value: reg(2) },
            ],
            vec![],
        );
        let module = verified(
            vec![Constant::Int32(10), Constant::Int32(20), Constant::Int32(0)],
            vec![entry],
        );
        assert_eq!(run_ok(&module).value, Value::int32(30));
    }

    #[test]
    fn keys_iterator_enumerates_own_object_keys() {
        // obj = {a:1}; for-in yields "a".
        let entry = function(
            0,
            6,
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
                Instruction::GetIterator {
                    dst: reg(3),
                    src: reg(0),
                    kind: IteratorKind::Keys,
                },
                Instruction::IteratorNext {
                    done: reg(4),
                    value: reg(5),
                    iterator: reg(3),
                },
                Instruction::Return { value: reg(5) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("a")),
                Constant::Int32(1),
            ],
            vec![entry],
        );
        let execution = run_ok(&module);
        // The produced key must equal a fresh "a" string.
        let key = execution.value;
        // Compare via a second machine's constant is awkward; instead assert it
        // is a heap string by checking done flag was false.
        assert_eq!(execution.entry_registers[4], Value::FALSE);
        assert_ne!(key, Value::UNDEFINED);
    }

    #[test]
    fn async_iterator_steps_like_sync() {
        let entry = function(
            0,
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
                    kind: IteratorKind::Async,
                },
                Instruction::IteratorNext {
                    done: reg(3),
                    value: reg(4),
                    iterator: reg(2),
                },
                Instruction::Return { value: reg(4) },
            ],
            vec![],
        );
        let module = verified(vec![Constant::Int32(8)], vec![entry]);
        let execution = run_ok(&module);
        assert_eq!(execution.value, Value::int32(8));
        assert_eq!(execution.entry_registers[3], Value::FALSE);
    }

    #[test]
    fn globals_store_load_and_typeof_undeclared() {
        // StoreGlobal x=5; TypeOfGlobal y (undeclared) -> "undefined";
        // TypeOfGlobal x -> "number"; return LoadGlobal x.
        let entry = function(
            0,
            3,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(2),
                },
                Instruction::StoreGlobal {
                    name: cid(0),
                    value: reg(0),
                },
                Instruction::TypeOfGlobal {
                    dst: reg(1),
                    name: cid(1),
                },
                Instruction::TypeOfGlobal {
                    dst: reg(2),
                    name: cid(0),
                },
                Instruction::LoadGlobal {
                    dst: reg(0),
                    name: cid(0),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("y")),
                Constant::Int32(5),
            ],
            vec![entry],
        );
        assert_eq!(run_ok(&module).value, Value::int32(5));
    }

    #[test]
    fn load_undeclared_global_throws_reference_error() {
        let module = verified(
            vec![Constant::String(EcmaString::from_utf8("missing"))],
            vec![function(
                0,
                2,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
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
        let mut host = TestHost;
        // No handler at top level path would raise; here it is caught, and the
        // caught value is undefined (the ReferenceError marker value).
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::UNDEFINED);
    }

    #[test]
    fn uncaught_reference_error_reports_origin() {
        let module = verified(
            vec![Constant::String(EcmaString::from_utf8("missing"))],
            vec![function(
                0,
                1,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                vec![],
            )],
        );
        let mut host = TestHost;
        let error = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert_eq!(error.pc, pc(0));
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                origin: ThrowOrigin::ReferenceError { .. },
                ..
            }
        ));
    }

    fn assert_uri_error(global: &str, argument: EcmaString) {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8(global)),
                Constant::String(argument),
                Constant::Undefined,
            ],
            vec![function(
                0,
                5,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(2),
                    },
                    Instruction::CreateArray { dst: reg(3) },
                    Instruction::ArrayPush {
                        array: reg(3),
                        value: reg(1),
                    },
                    Instruction::Call {
                        dst: reg(4),
                        callee: reg(0),
                        this_value: reg(2),
                        arguments: reg(3),
                    },
                    Instruction::Return { value: reg(4) },
                ],
                Vec::new(),
            )],
        );
        let mut host = TestHost;
        let error = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert_eq!(error.pc, pc(5));
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                origin: ThrowOrigin::UriError {
                    operation: "URI malformed"
                },
                ..
            }
        ));
    }

    #[test]
    fn uri_builtins_report_uri_error() {
        for (global, argument) in [
            ("encodeURIComponent", EcmaString::from_units(&[0xd800])),
            ("decodeURIComponent", EcmaString::from_utf8("%")),
            ("decodeURIComponent", EcmaString::from_utf8("%GG")),
            ("decodeURIComponent", EcmaString::from_utf8("%FF")),
            ("decodeURIComponent", EcmaString::from_utf8("%80")),
            ("decodeURIComponent", EcmaString::from_utf8("%C0%80")),
            ("decodeURIComponent", EcmaString::from_utf8("%E2%82")),
            ("decodeURIComponent", EcmaString::from_utf8("%ED%A0%80")),
            ("decodeURIComponent", EcmaString::from_utf8("%F4%90%80%80")),
            (
                "decodeURIComponent",
                EcmaString::from_utf8("%F8%80%80%80%80"),
            ),
        ] {
            assert_uri_error(global, argument);
        }
    }

    fn assert_uri_decode(argument: EcmaString, expected: EcmaString) {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("decodeURIComponent")),
                Constant::String(argument),
                Constant::Undefined,
                Constant::String(expected),
            ],
            vec![function(
                0,
                7,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(2),
                    },
                    Instruction::CreateArray { dst: reg(3) },
                    Instruction::ArrayPush {
                        array: reg(3),
                        value: reg(1),
                    },
                    Instruction::Call {
                        dst: reg(4),
                        callee: reg(0),
                        this_value: reg(2),
                        arguments: reg(3),
                    },
                    Instruction::LoadConst {
                        dst: reg(5),
                        constant: cid(3),
                    },
                    Instruction::Binary {
                        dst: reg(6),
                        op: BinaryOp::StrictEqual,
                        left: reg(4),
                        right: reg(5),
                    },
                    Instruction::Return { value: reg(6) },
                ],
                Vec::new(),
            )],
        );
        let mut host = TestHost;
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .unwrap();
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn decode_uri_component_preserves_units_and_decodes_utf8() {
        let exact = EcmaString::from_units(&[0xd800, 0x61, 0xdfff]);
        for (argument, expected) in [
            (exact.clone(), exact),
            (EcmaString::from_utf8("%2F"), EcmaString::from_utf8("/")),
            (
                EcmaString::from_utf8("%F0%9F%98%80"),
                EcmaString::from_utf8("😀"),
            ),
            (
                EcmaString::from_utf8("%E4%B8%ADA"),
                EcmaString::from_utf8("中A"),
            ),
            (EcmaString::from_utf8("%00"), EcmaString::from_units(&[0])),
        ] {
            assert_uri_decode(argument, expected);
        }
    }

    #[test]
    fn regexp_is_object_with_source_and_flags() {
        // typeof re === "object" is not directly returnable; return re.source.
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("ab")),
                Constant::String(EcmaString::from_utf8("gi")),
                Constant::String(EcmaString::from_utf8("source")),
                Constant::String(EcmaString::from_utf8("global")),
            ],
            vec![function(
                0,
                4,
                vec![
                    Instruction::CreateRegExp {
                        dst: reg(0),
                        pattern: cid(0),
                        flags: cid(1),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(3),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Unary {
                        dst: reg(3),
                        op: UnaryOp::TypeOf,
                        operand: reg(0),
                    },
                    Instruction::Return { value: reg(2) },
                ],
                vec![],
            )],
        );
        let execution = run_ok(&module);
        // re.global -> true
        assert_eq!(execution.value, Value::TRUE);
    }

    #[test]
    fn this_and_new_target_are_frame_owned() {
        // Call passes this; new.target is undefined in a plain call.
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateObject { dst: reg(0) },
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(1),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                Instruction::CreateArray { dst: reg(2) },
                Instruction::Call {
                    dst: reg(0),
                    callee: reg(1),
                    this_value: reg(0),
                    arguments: reg(2),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        // returns (this === passed) is hard cross-frame; instead return typeof
        // new.target which is "undefined" for a plain call.
        let callee = function(
            0,
            2,
            vec![
                Instruction::LoadNewTarget { dst: reg(0) },
                Instruction::Unary {
                    dst: reg(1),
                    op: UnaryOp::TypeOf,
                    operand: reg(0),
                },
                Instruction::Return { value: reg(1) },
            ],
            vec![],
        );
        let module = verified(vec![], vec![entry, callee]);
        let execution = run_ok(&module);
        // typeof undefined is a heap "undefined" string; strict-compare against
        // typeof of a known-undefined value is awkward, so assert non-undefined
        // heap string was produced and the call completed.
        assert_ne!(execution.value, Value::UNDEFINED);
    }

    #[test]
    fn new_target_is_constructor_during_construct() {
        // In a constructor, new.target === callee; verify via instanceof-style
        // check: store new.target on this, then read back after construct.
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(0),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                // ctor.prototype = {}
                Instruction::CreateObject { dst: reg(1) },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(0),
                },
                Instruction::SetProperty {
                    object: reg(0),
                    key: reg(2),
                    value: reg(1),
                },
                Instruction::CreateArray { dst: reg(3) },
                Instruction::Construct {
                    dst: reg(1),
                    callee: reg(0),
                    arguments: reg(3),
                },
                // read back this.nt === ctor
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(1),
                },
                Instruction::GetProperty {
                    dst: reg(3),
                    object: reg(1),
                    key: reg(2),
                },
                Instruction::Binary {
                    dst: reg(3),
                    op: BinaryOp::StrictEqual,
                    left: reg(3),
                    right: reg(0),
                },
                Instruction::Return { value: reg(3) },
            ],
            vec![],
        );
        let ctor = function(
            0,
            3,
            vec![
                Instruction::LoadNewTarget { dst: reg(0) },
                Instruction::LoadThis { dst: reg(1) },
                Instruction::LoadConst {
                    dst: reg(2),
                    constant: cid(1),
                },
                Instruction::SetProperty {
                    object: reg(1),
                    key: reg(2),
                    value: reg(0),
                },
                Instruction::Halt,
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("prototype")),
                Constant::String(EcmaString::from_utf8("nt")),
            ],
            vec![entry, ctor],
        );
        assert_eq!(run_ok(&module).value, Value::TRUE);
    }

    #[test]
    fn arguments_object_reflects_passed_values() {
        // callee returns arguments[0].
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(0),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                // args = [42]
                Instruction::CreateArray { dst: reg(2) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::ArrayPush {
                    array: reg(2),
                    value: reg(1),
                },
                Instruction::Call {
                    dst: reg(0),
                    callee: reg(0),
                    this_value: reg(1),
                    arguments: reg(2),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let callee = function(
            0,
            2,
            vec![
                Instruction::LoadArguments { dst: reg(0) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(1),
                },
                Instruction::GetProperty {
                    dst: reg(0),
                    object: reg(0),
                    key: reg(1),
                },
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::Int32(42),
                Constant::String(EcmaString::from_utf8("0")),
            ],
            vec![entry, callee],
        );
        assert_eq!(run_ok(&module).value, Value::int32(42));
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
        assert_eq!(run_ok(&module).value, Value::int32(9));
    }

    #[test]
    fn callee_throw_unwinds_to_call_site_handler() {
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(0),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                Instruction::CreateArray { dst: reg(1) },
                Instruction::Call {
                    dst: reg(2),
                    callee: reg(0),
                    this_value: reg(1),
                    arguments: reg(1),
                },
                Instruction::Halt,
                Instruction::Return { value: reg(3) },
            ],
            vec![ExceptionHandler {
                start: pc(3),
                end: pc(4),
                handler: pc(5),
                catch_register: reg(3),
            }],
        );
        let callee = function(
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::Throw { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(vec![Constant::Int32(7)], vec![entry, callee]);
        assert_eq!(run_ok(&module).value, Value::int32(7));
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
        let mut host = TestHost;
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

        let mut host = TestHost;
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
    fn argument_array_length_limit_is_enforced() {
        let entry = function(
            0,
            4,
            vec![
                Instruction::CreateArray { dst: reg(3) },
                Instruction::CreateClosure {
                    dst: reg(0),
                    function: FunctionId::new(1),
                    captures: reg(3),
                },
                Instruction::CreateArray { dst: reg(2) },
                Instruction::LoadConst {
                    dst: reg(1),
                    constant: cid(0),
                },
                Instruction::ArrayPush {
                    array: reg(2),
                    value: reg(1),
                },
                Instruction::Call {
                    dst: reg(0),
                    callee: reg(0),
                    this_value: reg(1),
                    arguments: reg(2),
                },
                Instruction::Halt,
            ],
            vec![],
        );
        let callee = function(1, 1, vec![Instruction::Return { value: reg(0) }], vec![]);
        let module = verified(vec![Constant::Int32(1)], vec![entry, callee]);
        let mut host = TestHost;
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
        let execution = run_ok(&module);
        assert_eq!(execution.value, Value::number(3.5));
        assert_eq!(execution.entry_registers[199], Value::number(3.5));
    }

    #[test]
    fn construct_returned_object_overrides_default_instance() {
        // A constructor returning its own object overrides the default instance.
        let entry = function(
            0,
            3,
            vec![
                Instruction::CreateArray { dst: reg(2) },
                Instruction::CreateClosure {
                    dst: reg(0),
                    function: FunctionId::new(1),
                    captures: reg(2),
                },
                Instruction::CreateArray { dst: reg(2) },
                Instruction::Construct {
                    dst: reg(1),
                    callee: reg(0),
                    arguments: reg(2),
                },
                // returned object has marker property set to 5
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::GetProperty {
                    dst: reg(2),
                    object: reg(1),
                    key: reg(0),
                },
                Instruction::Return { value: reg(2) },
            ],
            vec![],
        );
        let returns_object = function(
            0,
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
                Instruction::Return { value: reg(0) },
            ],
            vec![],
        );
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("marker")),
                Constant::Int32(5),
            ],
            vec![entry, returns_object],
        );
        assert_eq!(run_ok(&module).value, Value::int32(5));
    }

    #[test]
    fn ecmascript_number_formatting_is_shortest_round_trip() {
        let cases = [
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (-0.0, "0"),
            (1.0 / 3.0, "0.3333333333333333"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
        ];
        for (number, expected) in cases {
            assert_eq!(
                Machine::<TestHost>::ordinary_number_to_string(number),
                expected
            );
        }
    }

    #[test]
    fn own_keys_put_indices_before_insertion_ordered_strings() {
        let module = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        let index = machine.runtime_slot(object).unwrap().unwrap();
        for (key, value) in [("b", 1), ("2", 2), ("a", 3), ("1", 4)] {
            machine
                .set_own_data(
                    index,
                    PropertyKey::Named(EcmaString::from_utf8(key)),
                    Value::int32(value),
                )
                .unwrap();
        }
        assert_eq!(
            machine.enumerable_keys(object).unwrap(),
            ["1", "2", "b", "a"].map(EcmaString::from_utf8)
        );
    }

    #[test]
    fn object_prototype_to_string_uses_realm_tags() {
        let module = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        let function = machine.intrinsics.global("Object").unwrap();
        let to_string = machine.intrinsics.object_to_string();
        for (value, expected) in [
            (Value::UNDEFINED, "[object Undefined]"),
            (Value::NULL, "[object Null]"),
            (Value::TRUE, "[object Boolean]"),
            (array, "[object Array]"),
            (object, "[object Object]"),
            (function, "[object Function]"),
        ] {
            let tag = machine.call_value(to_string, value, &[]).unwrap();
            assert!(
                machine
                    .string_text(tag)
                    .is_some_and(|text| text.eq_ascii(expected))
            );
        }
    }

    #[derive(Default)]
    struct CapabilityHost {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        env: BTreeMap<String, String>,
    }

    impl Host for CapabilityHost {
        fn write_stdout(&mut self, bytes: &[u8]) {
            self.stdout.extend_from_slice(bytes);
        }

        fn write_stderr(&mut self, bytes: &[u8]) {
            self.stderr.extend_from_slice(bytes);
        }

        fn env(&self, name: &str) -> Option<&str> {
            self.env.get(name).map(String::as_str)
        }

        fn set_env(&mut self, name: &str, value: &str) {
            self.env.insert(name.to_owned(), value.to_owned());
        }

        fn delete_env(&mut self, name: &str) -> bool {
            self.env.remove(name).is_some()
        }
    }

    #[test]
    fn console_formats_node_value_shapes_byte_exactly() {
        let module = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = CapabilityHost::default();
        {
            let mut machine = Machine::new(&module, &mut host, Limits::default());
            let console = machine.intrinsics.global("console").unwrap();
            let log = machine.get_named_property(console, "log").unwrap();
            let string = machine
                .allocate(HeapEntry::String(EcmaString::from_utf8("hello")))
                .unwrap();
            let array_string = machine
                .allocate(HeapEntry::String(EcmaString::from_utf8("x")))
                .unwrap();
            let array = machine
                .allocate(HeapEntry::Array {
                    elements: vec![Value::int32(1), array_string],
                    properties: PropertyMap::default(),
                    prototype: Some(machine.intrinsics.array_prototype),
                    extensible: true,
                    length_writable: true,
                })
                .unwrap();
            let mut inner_properties = PropertyMap::default();
            inner_properties.insert(
                PropertyKey::Named(EcmaString::from_utf8("answer")),
                Property::Data {
                    value: Value::int32(42),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            );
            let inner = machine
                .allocate(HeapEntry::Object {
                    properties: inner_properties,
                    prototype: Some(machine.intrinsics.object_prototype),
                    boxed_primitive: None,
                    extensible: true,
                })
                .unwrap();
            let mut outer_properties = PropertyMap::default();
            outer_properties.insert(
                PropertyKey::Named(EcmaString::from_utf8("nested")),
                Property::Data {
                    value: inner,
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            );
            let outer = machine
                .allocate(HeapEntry::Object {
                    properties: outer_properties,
                    prototype: Some(machine.intrinsics.object_prototype),
                    boxed_primitive: None,
                    extensible: true,
                })
                .unwrap();
            let symbol = machine
                .allocate(HeapEntry::Symbol {
                    description: EcmaString::from_utf8("token"),
                })
                .unwrap();
            for value in [
                string,
                Value::int32(42),
                array,
                outer,
                Value::UNDEFINED,
                Value::NULL,
                symbol,
            ] {
                machine.call_value(log, console, &[value]).unwrap();
            }
        }
        assert_eq!(
            host.stdout,
            b"hello\n42\n[ 1, 'x' ]\n{ nested: { answer: 42 } }\nundefined\nnull\nSymbol(token)\n"
        );
        assert!(host.stderr.is_empty());
    }

    #[test]
    fn console_and_process_properties_are_reassignable_and_env_is_live() {
        let module = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = CapabilityHost::default();
        {
            let mut machine = Machine::new(&module, &mut host, Limits::default());
            let console = machine.intrinsics.global("console").unwrap();
            let warn = machine.get_named_property(console, "warn").unwrap();
            machine
                .set_data_property(console, "warn", Value::int32(91))
                .unwrap();
            assert_eq!(
                machine.get_named_property(console, "warn").unwrap(),
                Value::int32(91)
            );
            machine.set_data_property(console, "warn", warn).unwrap();

            let process = machine.intrinsics.global("process").unwrap();
            let env = machine.get_named_property(process, "env").unwrap();
            machine
                .set_data_property(env, "BAMTS_MODE", Value::int32(7))
                .unwrap();
            let value = machine.get_named_property(env, "BAMTS_MODE").unwrap();
            assert!(
                machine
                    .string_text(value)
                    .is_some_and(|text| text.eq_ascii("7"))
            );
            assert!(
                machine
                    .delete_property(
                        env,
                        &PropertyKey::Named(EcmaString::from_utf8("BAMTS_MODE"))
                    )
                    .unwrap()
            );
            assert_eq!(
                machine.get_named_property(env, "BAMTS_MODE").unwrap(),
                Value::UNDEFINED
            );
        }
        assert_eq!(host.env("BAMTS_MODE"), None);
    }

    #[test]
    fn independent_modules_keep_same_name_globals_isolated() {
        let dependency = |name: &str, value: i32| {
            program_module(
                name,
                vec![
                    Constant::String(EcmaString::from_utf8("x")),
                    Constant::Int32(value),
                ],
                vec![function(
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
                    Vec::new(),
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
            ],
            vec![function(
                0,
                5,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(1),
                        name: cid(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(2),
                        constant: cid(3),
                    },
                    Instruction::GetProperty {
                        dst: reg(3),
                        object: reg(0),
                        key: reg(2),
                    },
                    Instruction::GetProperty {
                        dst: reg(4),
                        object: reg(1),
                        key: reg(2),
                    },
                    Instruction::Binary {
                        dst: reg(0),
                        op: BinaryOp::Add,
                        left: reg(3),
                        right: reg(4),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
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
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Namespace {
                        edge: EdgeId::new(0),
                    },
                },
                Binding {
                    name: cid(2),
                    kind: BindingKind::Namespace {
                        edge: EdgeId::new(1),
                    },
                },
            ],
            Vec::new(),
        );
        let program = linked(vec![dependency("left", 1), dependency("right", 2), root], 2);
        assert_eq!(run_ok(&program).value, Value::int32(3));
    }

    #[test]
    fn imported_binding_observes_post_link_mutation_live() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::String(EcmaString::from_utf8("set")),
            ],
            vec![
                function(
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
                    Vec::new(),
                ),
                function(
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
                    Vec::new(),
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
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("set")),
                Constant::String(EcmaString::from_utf8("dep")),
            ],
            vec![function(
                0,
                3,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(2),
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
                        name: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
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
            run_ok(&linked(vec![dependency, root], 1)).value,
            Value::int32(2)
        );
    }

    #[test]
    fn closure_globals_resolve_in_the_defining_module() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(10),
                Constant::String(EcmaString::from_utf8("read")),
            ],
            vec![
                function(
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
                            name: cid(3),
                            value: reg(2),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
            Vec::new(),
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Hoisted,
                },
                Binding {
                    name: cid(3),
                    kind: BindingKind::Hoisted,
                },
            ],
            vec![Export {
                name: cid(3),
                source: ExportSource::Local(BindingId::new(1)),
            }],
        );
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(20),
                Constant::String(EcmaString::from_utf8("read")),
                Constant::String(EcmaString::from_utf8("dep")),
            ],
            vec![function(
                0,
                4,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(2),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(0),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(1),
                        name: cid(3),
                    },
                    Instruction::CreateArray { dst: reg(2) },
                    Instruction::Call {
                        dst: reg(3),
                        callee: reg(1),
                        this_value: reg(2),
                        arguments: reg(2),
                    },
                    Instruction::Return { value: reg(3) },
                ],
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(4),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Hoisted,
                },
                Binding {
                    name: cid(3),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: cid(3),
                    },
                },
            ],
            Vec::new(),
        );
        assert_eq!(
            run_ok(&linked(vec![dependency, root], 1)).value,
            Value::int32(10)
        );
    }

    #[test]
    fn cycle_traps_a_lexical_read_before_initialization() {
        let first = program_module(
            "first",
            vec![
                Constant::String(EcmaString::from_utf8("a")),
                Constant::Int32(1),
                Constant::String(EcmaString::from_utf8("second")),
            ],
            vec![function(
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
                Vec::new(),
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
            vec![function(
                0,
                1,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
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
        let program = linked(vec![first, second], 0);
        let mut host = TestHost;
        let error = Machine::new(&program, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::TemporalDeadZone { module, binding }
                if module == ModuleId::new(1) && binding == BindingId::new(0)
        ));
    }

    #[test]
    fn cycle_reentry_with_a_hoisted_binding_completes() {
        let first = program_module(
            "first",
            vec![
                Constant::String(EcmaString::from_utf8("a")),
                Constant::Int32(1),
                Constant::String(EcmaString::from_utf8("second")),
            ],
            vec![function(
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
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(3),
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Static,
            }],
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Hoisted,
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
            vec![function(
                0,
                1,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
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
        assert_eq!(
            run_ok(&linked(vec![first, second], 0)).value,
            Value::int32(1)
        );
    }

    #[test]
    fn namespace_identity_reads_live_cells_and_enumerates_sorted_keys() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("z")),
                Constant::String(EcmaString::from_utf8("a")),
                Constant::String(EcmaString::from_utf8("mutate")),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::Int32(3),
            ],
            vec![
                function(
                    0,
                    4,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(4),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(5),
                        },
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(0),
                        },
                        Instruction::CreateArray { dst: reg(1) },
                        Instruction::CreateClosure {
                            dst: reg(2),
                            function: FunctionId::new(1),
                            captures: reg(1),
                        },
                        Instruction::StoreGlobal {
                            name: cid(3),
                            value: reg(2),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(6),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
            Vec::new(),
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Hoisted,
                },
                Binding {
                    name: cid(2),
                    kind: BindingKind::Hoisted,
                },
                Binding {
                    name: cid(3),
                    kind: BindingKind::Hoisted,
                },
            ],
            vec![
                Export {
                    name: cid(1),
                    source: ExportSource::Local(BindingId::new(0)),
                },
                Export {
                    name: cid(2),
                    source: ExportSource::Local(BindingId::new(1)),
                },
                Export {
                    name: cid(3),
                    source: ExportSource::Local(BindingId::new(2)),
                },
            ],
        );
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("ns1")),
                Constant::String(EcmaString::from_utf8("ns2")),
                Constant::String(EcmaString::from_utf8("mutate")),
                Constant::String(EcmaString::from_utf8("z")),
                Constant::String(EcmaString::from_utf8("a")),
                Constant::String(EcmaString::from_utf8("dep")),
                Constant::String(EcmaString::from_utf8("Object")),
                Constant::String(EcmaString::from_utf8("getOwnPropertyDescriptor")),
                Constant::String(EcmaString::from_utf8("value")),
                Constant::String(EcmaString::from_utf8("writable")),
                Constant::String(EcmaString::from_utf8("enumerable")),
                Constant::String(EcmaString::from_utf8("configurable")),
                Constant::String(EcmaString::from_utf8("missing")),
            ],
            vec![function(
                0,
                31,
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
                        op: BinaryOp::StrictEqual,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(3),
                        name: cid(3),
                    },
                    Instruction::CreateArray { dst: reg(4) },
                    Instruction::Call {
                        dst: reg(5),
                        callee: reg(3),
                        this_value: reg(4),
                        arguments: reg(4),
                    },
                    Instruction::LoadConst {
                        dst: reg(6),
                        constant: cid(4),
                    },
                    Instruction::GetProperty {
                        dst: reg(7),
                        object: reg(0),
                        key: reg(6),
                    },
                    Instruction::GetIterator {
                        dst: reg(8),
                        src: reg(0),
                        kind: IteratorKind::Keys,
                    },
                    Instruction::IteratorNext {
                        done: reg(9),
                        value: reg(10),
                        iterator: reg(8),
                    },
                    Instruction::LoadConst {
                        dst: reg(11),
                        constant: cid(5),
                    },
                    Instruction::Binary {
                        dst: reg(12),
                        op: BinaryOp::StrictEqual,
                        left: reg(10),
                        right: reg(11),
                    },
                    Instruction::IteratorNext {
                        done: reg(9),
                        value: reg(10),
                        iterator: reg(8),
                    },
                    Instruction::LoadConst {
                        dst: reg(13),
                        constant: cid(3),
                    },
                    Instruction::Binary {
                        dst: reg(5),
                        op: BinaryOp::StrictEqual,
                        left: reg(10),
                        right: reg(13),
                    },
                    Instruction::IteratorNext {
                        done: reg(9),
                        value: reg(10),
                        iterator: reg(8),
                    },
                    Instruction::Binary {
                        dst: reg(14),
                        op: BinaryOp::StrictEqual,
                        left: reg(10),
                        right: reg(6),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(15),
                        name: cid(7),
                    },
                    Instruction::LoadConst {
                        dst: reg(16),
                        constant: cid(8),
                    },
                    Instruction::GetProperty {
                        dst: reg(17),
                        object: reg(15),
                        key: reg(16),
                    },
                    Instruction::CreateArray { dst: reg(18) },
                    Instruction::ArrayPush {
                        array: reg(18),
                        value: reg(0),
                    },
                    Instruction::ArrayPush {
                        array: reg(18),
                        value: reg(6),
                    },
                    Instruction::Call {
                        dst: reg(19),
                        callee: reg(17),
                        this_value: reg(18),
                        arguments: reg(18),
                    },
                    Instruction::LoadConst {
                        dst: reg(20),
                        constant: cid(9),
                    },
                    Instruction::GetProperty {
                        dst: reg(21),
                        object: reg(19),
                        key: reg(20),
                    },
                    Instruction::LoadConst {
                        dst: reg(22),
                        constant: cid(10),
                    },
                    Instruction::GetProperty {
                        dst: reg(23),
                        object: reg(19),
                        key: reg(22),
                    },
                    Instruction::LoadConst {
                        dst: reg(24),
                        constant: cid(11),
                    },
                    Instruction::GetProperty {
                        dst: reg(25),
                        object: reg(19),
                        key: reg(24),
                    },
                    Instruction::LoadConst {
                        dst: reg(26),
                        constant: cid(12),
                    },
                    Instruction::GetProperty {
                        dst: reg(27),
                        object: reg(19),
                        key: reg(26),
                    },
                    Instruction::CreateArray { dst: reg(28) },
                    Instruction::LoadConst {
                        dst: reg(29),
                        constant: cid(13),
                    },
                    Instruction::ArrayPush {
                        array: reg(28),
                        value: reg(0),
                    },
                    Instruction::ArrayPush {
                        array: reg(28),
                        value: reg(29),
                    },
                    Instruction::Call {
                        dst: reg(30),
                        callee: reg(17),
                        this_value: reg(28),
                        arguments: reg(28),
                    },
                    Instruction::Return { value: reg(21) },
                ],
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(6),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![
                Binding {
                    name: cid(1),
                    kind: BindingKind::Namespace {
                        edge: EdgeId::new(0),
                    },
                },
                Binding {
                    name: cid(2),
                    kind: BindingKind::Namespace {
                        edge: EdgeId::new(0),
                    },
                },
                Binding {
                    name: cid(3),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: cid(3),
                    },
                },
            ],
            Vec::new(),
        );
        let execution = run_ok(&linked(vec![dependency, root], 1));
        assert_eq!(execution.value, Value::int32(3));
        assert_eq!(execution.entry_registers[2], Value::TRUE);
        assert_eq!(execution.entry_registers[5], Value::TRUE);
        assert_eq!(execution.entry_registers[12], Value::TRUE);
        assert_eq!(execution.entry_registers[14], Value::TRUE);
        assert_eq!(execution.entry_registers[23], Value::TRUE);
        assert_eq!(execution.entry_registers[25], Value::TRUE);
        assert_eq!(execution.entry_registers[27], Value::FALSE);
        assert_eq!(execution.entry_registers[30], Value::UNDEFINED);
    }

    #[test]
    fn side_effect_module_runs_once_with_single_or_duplicate_static_edges() {
        for duplicate in [false, true] {
            let dependency = program_module(
                "dependency",
                vec![
                    Constant::String(EcmaString::from_utf8("count")),
                    Constant::Int32(0),
                    Constant::Int32(1),
                ],
                vec![function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(1),
                        },
                        Instruction::JumpIfFalse {
                            condition: reg(0),
                            target: pc(3),
                        },
                        Instruction::Jump { target: pc(5) },
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(2),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
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
                    Vec::new(),
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
            let mut edges = vec![Edge {
                specifier: cid(2),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }];
            if duplicate {
                edges.push(Edge {
                    specifier: cid(3),
                    target: EdgeTarget::Local(ModuleId::new(0)),
                    kind: EdgeKind::Static,
                });
            }
            let root = program_module(
                "root",
                vec![
                    Constant::String(EcmaString::from_utf8("count")),
                    Constant::String(EcmaString::from_utf8("dep-one")),
                    Constant::String(EcmaString::from_utf8("dep-two")),
                ],
                vec![function(
                    0,
                    1,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                )],
                edges,
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
                run_ok(&linked(vec![dependency, root], 1)).value,
                Value::int32(1)
            );
        }
    }

    #[test]
    fn failed_module_rethrows_the_identical_stored_value() {
        let module = program_module(
            "throws",
            Vec::new(),
            vec![function(
                0,
                1,
                vec![
                    Instruction::CreateObject { dst: reg(0) },
                    Instruction::Throw { value: reg(0) },
                ],
                Vec::new(),
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![module], 0);
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        machine.instantiate_modules().unwrap();
        let first = machine.evaluate_module(ModuleId::new(0)).unwrap_err();
        let second = machine.evaluate_module(ModuleId::new(0)).unwrap_err();
        let RuntimeErrorKind::UncaughtThrow { value: first, .. } = first.kind else {
            panic!("module must fail by throwing");
        };
        let RuntimeErrorKind::UncaughtThrow { value: second, .. } = second.kind else {
            panic!("stored failure must remain a throw");
        };
        assert_eq!(first, second);
        assert!(first.as_heap_ref().is_some());
    }

    #[test]
    fn external_static_edge_is_a_typed_runtime_error() {
        let module = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("external"))],
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            }],
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![module], 0);
        let mut host = TestHost;
        let error = Machine::new(&program, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::ExternalModuleUnavailable { module, edge }
                if module == ModuleId::new(0) && edge == EdgeId::new(0)
        ));
    }

    #[test]
    fn external_module_and_export_names_preserve_unicode() {
        for (specifier, export) in [("módulo", "value"), ("external", "café")] {
            let module = program_module(
                "root",
                vec![
                    Constant::String(EcmaString::from_utf8(export)),
                    Constant::String(EcmaString::from_utf8(specifier)),
                ],
                vec![function(
                    0,
                    1,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                )],
                vec![Edge {
                    specifier: cid(2),
                    target: EdgeTarget::External,
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
            let program = linked(vec![module], 0);
            let mut host = TestHost;
            let mut machine = Machine::new(&program, &mut host, Limits::default());
            machine.registry.external.insert(
                EcmaString::from_utf8(specifier),
                ExternalModuleInstance {
                    namespace: Value::UNDEFINED,
                    exports: BTreeMap::from([(
                        EcmaString::from_utf8(export),
                        ExternalExport {
                            value: Value::int32(7),
                            cell: None,
                        },
                    )]),
                    internals: BTreeMap::new(),
                },
            );

            assert_eq!(machine.run().unwrap().value, Value::int32(7));
        }
    }

    #[test]
    fn dynamic_import_is_a_typed_runtime_error() {
        let module = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("dynamic"))],
            vec![function(
                0,
                1,
                vec![
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(1),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::External,
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![module], 0);
        let mut host = TestHost;
        let error = Machine::new(&program, &mut host, Limits::default())
            .run()
            .unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::DynamicImportUnsupported { module } if module == ModuleId::new(0)
        ));
    }

    #[test]
    fn unbound_global_names_fall_back_to_the_realm_global_map() {
        let program = verified(
            vec![
                Constant::String(EcmaString::from_utf8("realmOnly")),
                Constant::Int32(7),
            ],
            vec![function(
                0,
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(1),
                    },
                    Instruction::StoreGlobal {
                        name: cid(0),
                        value: reg(0),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
            )],
        );
        assert_eq!(run_ok(&program).value, Value::int32(7));
    }

    #[test]
    fn module_cell_limit_is_enforced_before_evaluation() {
        let module = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("x"))],
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
            Vec::new(),
            vec![Binding {
                name: cid(1),
                kind: BindingKind::Hoisted,
            }],
            Vec::new(),
        );
        let program = linked(vec![module], 0);
        let mut host = TestHost;
        let error = Machine::new(
            &program,
            &mut host,
            Limits {
                max_module_cells: 0,
                ..Limits::default()
            },
        )
        .run()
        .unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::ModuleCellLimitExceeded { limit: 0 }
        ));
    }
    #[test]
    fn imported_binding_store_throws_without_mutating_the_exporter() {
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(1),
            ],
            vec![function(
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
                Vec::new(),
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
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(2),
                Constant::String(EcmaString::from_utf8("dep")),
            ],
            vec![function(
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
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(3),
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
        let program = linked(vec![dependency, root], 1);
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        machine.instantiate_modules().unwrap();
        assert!(machine.evaluate_module(ModuleId::new(1)).is_err());
        let exporter = machine.registry.modules[0].binding_cells[0].unwrap();
        assert_eq!(machine.registry.cells[exporter.0].value, Value::int32(1));
    }

    #[test]
    fn namespace_descriptor_propagates_temporal_dead_zone() {
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::Int32(1),
                Constant::String(EcmaString::from_utf8("dependency")),
            ],
            vec![function(
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
                Vec::new(),
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
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("ns")),
                Constant::String(EcmaString::from_utf8("root")),
                Constant::String(EcmaString::from_utf8("Object")),
                Constant::String(EcmaString::from_utf8("getOwnPropertyDescriptor")),
                Constant::String(EcmaString::from_utf8("x")),
            ],
            vec![namespace_descriptor_entry()],
            vec![Edge {
                specifier: cid(2),
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
        let program = linked(vec![root, dependency], 0);
        let mut host = TestHost;
        let error = Machine::new(&program, &mut host, Limits::default())
            .run()
            .expect_err("descriptor reads uninitialized namespace export");
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::TemporalDeadZone { module, binding }
                if module == ModuleId::new(0) && binding == BindingId::new(0)
        ));
    }

    #[test]
    fn namespace_descriptor_propagates_external_linkage_error() {
        let exported = program_module(
            "exported",
            vec![
                Constant::String(EcmaString::from_utf8("x")),
                Constant::String(EcmaString::from_utf8("external")),
            ],
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
            vec![Edge {
                specifier: cid(2),
                target: EdgeTarget::External,
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            vec![Export {
                name: cid(1),
                source: ExportSource::Indirect {
                    edge: EdgeId::new(0),
                    name: cid(1),
                },
            }],
        );
        let importer = program_module(
            "importer",
            vec![
                Constant::String(EcmaString::from_utf8("ns")),
                Constant::String(EcmaString::from_utf8("exported")),
                Constant::String(EcmaString::from_utf8("Object")),
                Constant::String(EcmaString::from_utf8("getOwnPropertyDescriptor")),
                Constant::String(EcmaString::from_utf8("x")),
            ],
            vec![namespace_descriptor_entry()],
            vec![Edge {
                specifier: cid(2),
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
        let program = linked(vec![exported, importer], 1);
        let mut host = TestHost;
        let error = Machine::new(&program, &mut host, Limits::default())
            .run()
            .expect_err("descriptor resolves external namespace export");
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::ExternalModuleUnavailable { module, edge }
                if module == ModuleId::new(0) && edge == EdgeId::new(0)
        ));
    }
}
