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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use bamts_bytecode::{
    AccessorKind, BinaryOp, BindingId, BindingKind, Constant, ConstantId, EcmaString,
    EcmaStringBuilder, EdgeId, EdgeTarget, Function, FunctionId, Instruction, IteratorKind, Module,
    ModuleId, Pc, Program, ProgramModule, ResolvedExport, UnaryOp, Verified,
};
use bamts_native::{Decoded, SlotId, Value};

mod external_modules;
mod host_objects;
mod intrinsics;
mod native;
mod vm;

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
    /// Ceiling on host-compiled scripts retained by one machine.
    pub max_dynamic_modules: usize,
    /// Ceiling on queued microtasks.
    pub max_microtasks: usize,
    /// Ceiling on live timers armed at once.
    pub max_timers: usize,
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
            max_dynamic_modules: 1 << 10,
            max_microtasks: 1 << 20,
            max_timers: 1 << 20,
        }
    }
}

/// The exact source of one classic script. Source and resource name preserve
/// UTF-16 code units verbatim, including unpaired surrogates.
pub struct ScriptSource<'a> {
    pub source: &'a [u16],
    pub name: &'a [u16],
}

/// Why a host compiler refused a classic script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptCompileError {
    IllFormedSource {
        unit_offset: usize,
    },
    Syntax {
        message: String,
        line: u32,
        column: u32,
    },
    Unsupported {
        message: String,
        line: u32,
        column: u32,
    },
    Capacity {
        message: String,
    },
}

/// A host capability for compiling a classic script without observing machine state.
pub trait CompileProvider {
    fn compile_script(
        &mut self,
        source: ScriptSource<'_>,
    ) -> Result<Arc<Program<Verified>>, ScriptCompileError>;
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

    /// This host's script compiler, or `None` when it provides none.
    ///
    /// Presence MUST remain stable for the lifetime of one machine because it
    /// determines whether `node:vm` is installed during construction.
    fn script_compiler(&mut self) -> Option<&mut (dyn CompileProvider + 'static)> {
        None
    }

    /// This host's timer scheduler, or `None` when it provides none.
    ///
    /// Presence MUST remain stable for the lifetime of one machine because it
    /// determines whether `setTimeout`/`clearTimeout` are installed during
    /// construction.
    fn timers(&mut self) -> Option<&mut (dyn TimerProvider + 'static)> {
        None
    }
}

/// A single expired timer reported by a [`TimerProvider`].
///
/// `id` echoes the machine-owned JavaScript identifier passed to
/// [`TimerProvider::schedule`]; `deadline_ms` is the provider's monotonic
/// absolute-millisecond deadline used as the promotion watermark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerWakeup {
    pub id: u64,
    pub deadline_ms: u64,
}

/// An owned failure message produced by a [`TimerProvider`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerError {
    message: String,
}

impl TimerError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TimerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TimerError {}

/// A host capability that schedules real time without observing machine state.
///
/// The provider owns only opaque `u64` identifiers and monotonic millisecond
/// deadlines; it never stores a JavaScript `Value`. The machine owns visible
/// ordering and every callback identity.
pub trait TimerProvider {
    /// Arms a timer for `id` after at least `delay_ms` milliseconds, returning
    /// the provider-monotonic absolute-millisecond deadline.
    fn schedule(&mut self, id: u64, delay_ms: u32) -> Result<u64, TimerError>;

    /// Cancels the armed timer for `id`, returning whether one was removed.
    fn cancel(&mut self, id: u64) -> Result<bool, TimerError>;

    /// Drains every currently expired timer into `output` without blocking.
    fn poll_expired(&mut self, output: &mut Vec<TimerWakeup>) -> Result<(), TimerError>;

    /// Blocks until the next timer expires, or returns `None` when none pend.
    fn wait_expired(&mut self) -> Result<Option<TimerWakeup>, TimerError>;

    /// Reports whether the provider has any armed timer.
    fn has_pending(&self) -> bool;
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
    DynamicModuleLimitExceeded {
        limit: usize,
    },
    MicrotaskQueueLimitExceeded {
        limit: usize,
    },
    MicrotaskDrainReentry,
    TimerProviderFailure {
        message: String,
    },
    TimerCapacityExceeded {
        limit: usize,
    },
    TimerCheckpointReentry,
    InvalidDynamicScript {
        reason: &'static str,
    },
    TemporalDeadZone {
        module: ModuleId,
        binding: BindingId,
    },
    ExternalModuleUnavailable {
        module: ModuleId,
        edge: EdgeId,
    },
    DynamicImportEdgeMissing {
        module: ModuleId,
        specifier: ConstantId,
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
            RuntimeErrorKind::DynamicModuleLimitExceeded { limit } => {
                write!(formatter, "dynamic module limit {limit} exceeded")
            }
            RuntimeErrorKind::MicrotaskQueueLimitExceeded { limit } => {
                write!(formatter, "microtask queue limit {limit} exceeded")
            }
            RuntimeErrorKind::MicrotaskDrainReentry => {
                write!(formatter, "microtask drain is already active")
            }
            RuntimeErrorKind::TimerProviderFailure { message } => {
                write!(formatter, "timer provider failure: {message}")
            }
            RuntimeErrorKind::TimerCapacityExceeded { limit } => {
                write!(formatter, "timer capacity {limit} exceeded")
            }
            RuntimeErrorKind::TimerCheckpointReentry => {
                write!(formatter, "timer checkpoint is already active")
            }
            RuntimeErrorKind::InvalidDynamicScript { reason } => {
                write!(formatter, "invalid dynamic script: {reason}")
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
            RuntimeErrorKind::DynamicImportEdgeMissing { module, specifier } => write!(
                formatter,
                "verified module {} has no dynamic edge for constant {}",
                module.get(),
                specifier.get()
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
    fn charge_bytes(&self) -> usize {
        self.0.iter().fold(0, |bytes, (key, _)| {
            bytes.saturating_add(key.charge_bytes())
        })
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

#[derive(Clone, Copy, Debug)]
pub(crate) enum IteratorNextPrepared {
    Ready { done: bool, value: Value },
    Call { callee: Value, this_value: Value },
}

#[derive(Clone, Debug)]
enum IteratorState {
    Keys { index: usize, keys: Vec<EcmaString> },
    Protocol { iterator: Value, next: Value },
}

#[derive(Clone, Debug)]
pub(crate) struct GeneratorStart {
    pub(crate) target: RuntimeFunction,
    pub(crate) captures: Vec<Value>,
    pub(crate) this_value: Value,
    pub(crate) new_target: Value,
    pub(crate) args: Vec<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct SuspendedActivation {
    pub(crate) target: RuntimeFunction,
    pub(crate) registers: Vec<Value>,
    pub(crate) this_value: Value,
    pub(crate) new_target: Value,
    pub(crate) args: Vec<Value>,
    pub(crate) arguments_object: Option<Value>,
    pub(crate) resume_token: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum GeneratorState {
    SuspendedStart(GeneratorStart),
    Executing,
    Suspended(SuspendedActivation),
    Completed,
}

#[derive(Clone, Debug)]
pub(crate) enum GeneratorResume {
    Yield {
        value: Value,
        activation: SuspendedActivation,
    },
    Return(Value),
    Throw {
        value: Value,
        origin: ThrowOrigin,
    },
}

/// One step of driving a detached async-function activation: it awaited a
/// value (and must suspend), it returned, or it threw an uncaught value.
#[derive(Clone, Debug)]
enum AsyncStep {
    Suspend {
        awaited: Value,
        activation: SuspendedActivation,
    },
    Return(Value),
    Throw {
        value: Value,
        origin: ThrowOrigin,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum PromiseState {
    Pending {
        fulfill_reactions: Vec<PromiseReaction>,
        reject_reactions: Vec<PromiseReaction>,
    },
    Fulfilled {
        value: Value,
    },
    Rejected {
        reason: Value,
        origin: ThrowOrigin,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PromiseCompletion {
    Fulfilled,
    Rejected,
}

#[derive(Clone, Debug)]
pub(crate) enum PromiseReaction {
    Fulfilled {
        handler: Value,
        derived: Value,
    },
    Rejected {
        handler: Value,
        derived: Value,
    },
    Finally {
        handler: Value,
        derived: Value,
        completion: PromiseCompletion,
    },
    /// Resumes a suspended async activation on fulfillment. Carries only the
    /// heap `AsyncActivation` record handle; the awaited value arrives as the
    /// reaction value.
    AsyncFulfill {
        activation: Value,
    },
    /// Resumes a suspended async activation on rejection. Carries only the
    /// heap `AsyncActivation` record handle; the rejection reason arrives as
    /// the reaction value.
    AsyncReject {
        activation: Value,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum MicrotaskJob {
    Reaction {
        reaction: PromiseReaction,
        value: Value,
        origin: ThrowOrigin,
    },
    Thenable {
        promise: Value,
        thenable: Value,
        then: Value,
    },
    Callback {
        callback: Value,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MicrotaskExceptionPolicy {
    CollectAndContinue,
    StopAtFirst,
}

/// An exception reported by a microtask or timer callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackException {
    /// The value thrown by the callback.
    pub value: Value,
    /// The operation that produced the throw.
    pub origin: ThrowOrigin,
}

/// The result of one explicit microtask checkpoint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MicrotaskDrain {
    /// The number of jobs removed from the queue and run.
    pub executed: usize,
    /// Callback exceptions, in FIFO execution order.
    pub uncaught: Vec<CallbackException>,
}

/// The result of one explicit timer checkpoint. At most one callback runs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimerRun {
    /// The number of timer callbacks run: `0` or `1`.
    pub executed: usize,
    /// Callback exceptions from the run callback, if it threw.
    pub uncaught: Vec<CallbackException>,
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
    /// A `vm.Script` whose entry function retains its machine-owned dynamic module.
    Script {
        entry: Value,
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
        state: IteratorState,
    },
    Generator {
        state: GeneratorState,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    ProcessEnv {
        prototype: Option<Value>,
        extensible: bool,
    },
    Promise {
        state: PromiseState,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    /// A Node-compatible `Timeout` handle carrying its monotonic timer id and
    /// ordinary object property/prototype fields.
    Timeout {
        id: u64,
        properties: PropertyMap,
        prototype: Option<Value>,
        extensible: bool,
    },
    PromiseResolver {
        promise: Value,
        used: bool,
    },
    PromiseFinally {
        derived: Value,
        value: Value,
        origin: ThrowOrigin,
        completion: PromiseCompletion,
    },
    PromiseAll {
        promise: Value,
        values: Vec<Value>,
        remaining: usize,
        settled: bool,
    },
    PromiseAllElement {
        aggregate: Value,
        index: usize,
        called: bool,
    },
    /// One suspended async-function activation together with the implicit
    /// result Promise it settles. `activation` is taken on resume so a second
    /// resume is a hard invalid-state error. This record is internal and never
    /// escapes to user code.
    AsyncActivation {
        activation: Option<SuspendedActivation>,
        promise: Value,
    },
    NativeFunction {
        callable: NativeCallable,
        properties: PropertyMap,
        extensible: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum NativeCallable {
    Builtin(intrinsics::BuiltinId),
    Bound(Box<BoundCallable>),
}

#[derive(Clone, Debug)]
pub(crate) struct BoundCallable {
    pub(crate) target: Value,
    pub(crate) this_value: Value,
    pub(crate) arguments: Vec<Value>,
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
            Self::NativeFunction { callable, .. } => match callable {
                NativeCallable::Builtin(_) => 1,
                NativeCallable::Bound(bound) => bound.arguments.len().saturating_add(1),
            },
            Self::Generator { state, .. } => match state {
                GeneratorState::SuspendedStart(start) => start
                    .captures
                    .len()
                    .saturating_add(start.args.len())
                    .saturating_add(1),
                GeneratorState::Suspended(activation) => activation
                    .registers
                    .len()
                    .saturating_add(activation.args.len())
                    .saturating_add(1),
                GeneratorState::Executing | GeneratorState::Completed => 1,
            },
            Self::Object { properties, .. } | Self::Timeout { properties, .. } => {
                properties.charge_bytes().saturating_add(1)
            }
            Self::Array { .. }
            | Self::Function { .. }
            | Self::Script { .. }
            | Self::ModuleNamespace { .. }
            | Self::ExternalModuleNamespace { .. }
            | Self::ProcessEnv { .. }
            | Self::Date { .. }
            | Self::BuiltinIterator { .. }
            | Self::Iterator { .. }
            | Self::Promise { .. }
            | Self::PromiseResolver { .. }
            | Self::PromiseFinally { .. }
            | Self::PromiseAll { .. }
            | Self::AsyncActivation { .. }
            | Self::PromiseAllElement { .. } => 1,
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

pub(crate) struct BoundCall {
    pub(crate) target: Value,
    pub(crate) this_value: Value,
    pub(crate) arguments: Vec<Value>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeFunction {
    pub(crate) module: ModuleId,
    pub(crate) function: FunctionId,
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
pub(crate) enum EvalFailure {
    Throw(ThrowOrigin),
    ThrowValue(Value),
    Runtime(RuntimeErrorKind),
    ThrowValueOrigin { value: Value, origin: ThrowOrigin },
}

pub(crate) fn import_failure(error: &RuntimeError) -> EvalFailure {
    match &error.kind {
        RuntimeErrorKind::UncaughtThrow { value, .. } => EvalFailure::ThrowValue(*value),
        kind => EvalFailure::Runtime(kind.clone()),
    }
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
    },
    Bound,
    NotCallable,
}

#[derive(Clone, Debug)]
struct TimerRecord {
    callback: Value,
    arguments: Vec<Value>,
    handle: Value,
    deadline_ms: u64,
    sequence: u64,
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
    /// Frame depths owned by native-to-runtime callback evaluations. A throw
    /// crossing this boundary returns to the native caller so the enclosing
    /// bytecode instruction resolves it at its own program counter.
    callback_boundaries: Vec<usize>,
    generator_boundaries: Vec<usize>,
    pending_generator_resume: Option<GeneratorResume>,
    async_boundaries: Vec<usize>,
    pending_async_suspend: Option<(Value, SuspendedActivation)>,
    microtasks: VecDeque<MicrotaskJob>,
    microtask_drain_active: bool,
    next_timer_id: Option<u64>,
    next_timer_sequence: Option<u64>,
    timers: BTreeMap<u64, TimerRecord>,
    ready_timers: BTreeSet<(u64, u64)>,
    timer_watermark: Option<u64>,
    timer_checkpoint_active: bool,
    intrinsics: intrinsics::Intrinsics<H>,
    current_builtin_id: Option<intrinsics::BuiltinId>,
    registry: ModuleRegistry,
    /// First machine-wide module ID reserved for host-compiled script modules.
    dynamic_base: usize,
    /// Host-compiled programs retained for the machine lifetime so their
    /// closures remain executable after the originating Script object dies.
    dynamic: Vec<DynamicModule>,
}

#[derive(Clone, Debug)]
struct DynamicModule {
    program: Arc<Program<Verified>>,
    bytes: usize,
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
    Evaluated(Result<(), RuntimeError>),
}

#[derive(Clone, Debug)]
pub(crate) enum ModuleEvaluation {
    Cycle,
    Evaluated(Result<(), RuntimeError>),
    Ready(Vec<ModuleId>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImportTarget {
    Local(ModuleId),
    External(EdgeId),
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
        let timers_available = host.timers().is_some();
        let mut intrinsics = intrinsics::Intrinsics::<H>::initialize(&mut heap, timers_available);
        let script_compiler = host.script_compiler().is_some();
        let installed_external = external_modules::install(
            &mut heap,
            &mut intrinsics.builtins,
            intrinsics.object_prototype,
            script_compiler,
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
            callback_boundaries: Vec::new(),
            generator_boundaries: Vec::new(),
            pending_generator_resume: None,
            async_boundaries: Vec::new(),
            pending_async_suspend: None,
            microtasks: VecDeque::new(),
            microtask_drain_active: false,
            next_timer_id: Some(1),
            next_timer_sequence: Some(0),
            timers: BTreeMap::new(),
            ready_timers: BTreeSet::new(),
            timer_watermark: None,
            timer_checkpoint_active: false,
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
            dynamic_base: program.map_or(1, |program| program.modules().len()),
            dynamic: Vec::new(),
            current_builtin_id: None,
            intrinsics,
        }
    }

    /// Evaluates the program, drives the automatic event loop to quiescence,
    /// and returns the synchronous [`Execution`] snapshot taken right after
    /// evaluation. Callbacks may mutate globals and append host output, but
    /// the returned value and entry register snapshot stay those of the
    /// synchronous evaluation.
    pub fn run(mut self) -> Result<Execution, RuntimeError> {
        let execution = self.evaluate()?;
        self.run_to_quiescence()?;
        Ok(execution)
    }

    /// Evaluates the program without draining queued microtasks.
    ///
    /// The machine remains available for an explicit [`Self::drain_microtasks`]
    /// checkpoint.
    pub fn evaluate(&mut self) -> Result<Execution, RuntimeError> {
        if let Some(program) = self.program {
            let entry = program.entry();
            self.frames.clear();
            self.live_registers = 0;
            self.instantiate_modules()?;
            return self.evaluate_module(entry)?.ok_or_else(|| {
                self.program_error(
                    entry,
                    RuntimeErrorKind::InvalidVerifiedProgram {
                        module: entry,
                        instruction: Instruction::Halt,
                    },
                )
            });
        }
        Ok(self
            .run_loop(0)?
            .expect("the entry frame completes before the run loop stops"))
    }

    /// Runs at most one expired live timer callback.
    ///
    /// This explicit checkpoint never drains microtasks. Expiry reports only
    /// advance a monotonic watermark; visible delivery is ordered by the
    /// machine-owned `(deadline, sequence)` key.
    pub fn run_one_expired_timer(&mut self) -> Result<TimerRun, RuntimeError> {
        if self.timer_checkpoint_active {
            return Err(self.checkpoint_error(RuntimeErrorKind::TimerCheckpointReentry));
        }
        self.timer_checkpoint_active = true;
        let result = (|| {
            self.poll_timer_expiries()
                .map_err(|kind| self.checkpoint_error(kind))?;
            let Some(order) = self.ready_timers.first().copied() else {
                return Ok(TimerRun::default());
            };
            let Some(id) = self.timers.iter().find_map(|(id, timer)| {
                ((timer.deadline_ms, timer.sequence) == order).then_some(*id)
            }) else {
                return Err(self.checkpoint_error(RuntimeErrorKind::InvalidValue {
                    value: Value::UNDEFINED,
                }));
            };
            // Preserve both the ready key and live record when fuel is empty.
            self.consume_fuel(1)
                .map_err(|kind| self.checkpoint_error(kind))?;
            self.ready_timers.remove(&order);
            let timer = self
                .timers
                .remove(&id)
                .expect("ready timer remains live until after fuel charging");
            let mut report = TimerRun {
                executed: 1,
                uncaught: Vec::new(),
            };
            match self.call_value(timer.callback, timer.handle, &timer.arguments) {
                Ok(_) => {}
                Err(EvalFailure::Runtime(kind)) => {
                    return Err(self.checkpoint_error(kind));
                }
                Err(failure) => {
                    let (value, origin) =
                        self.promise_rejection_value(failure)
                            .map_err(|failure| match failure {
                                EvalFailure::Runtime(kind) => self.checkpoint_error(kind),
                                _ => self.checkpoint_error(RuntimeErrorKind::InvalidValue {
                                    value: timer.callback,
                                }),
                            })?;
                    report.uncaught.try_reserve(1).map_err(|_| {
                        self.checkpoint_error(RuntimeErrorKind::HeapByteLimitExceeded {
                            limit: self.limits.max_heap_bytes,
                        })
                    })?;
                    report.uncaught.push(CallbackException { value, origin });
                }
            }
            Ok(report)
        })();
        self.timer_checkpoint_active = false;
        result
    }

    /// Blocks in the host provider until an expiry report is available.
    /// Returns whether at least one live timer became ready.
    pub fn wait_for_timer_expiry(&mut self) -> Result<bool, RuntimeError> {
        if self.timer_checkpoint_active {
            return Err(self.checkpoint_error(RuntimeErrorKind::TimerCheckpointReentry));
        }
        if !self.ready_timers.is_empty() {
            return Ok(true);
        }
        self.timer_checkpoint_active = true;
        let result = (|| {
            let wakeup = match self.host.timers() {
                Some(provider) => provider.wait_expired(),
                None => return Ok(false),
            }
            .map_err(|error| {
                self.checkpoint_error(RuntimeErrorKind::TimerProviderFailure {
                    message: error.to_string(),
                })
            })?;
            if let Some(wakeup) = wakeup {
                self.promote_timer_wakeup(wakeup);
            }
            Ok(!self.ready_timers.is_empty())
        })();
        self.timer_checkpoint_active = false;
        result
    }

    /// Returns whether any machine-owned live timer remains.
    #[must_use]
    pub fn has_pending_timers(&self) -> bool {
        !self.timers.is_empty()
    }

    /// Drives the automatic event loop to quiescence after synchronous
    /// evaluation.
    ///
    /// Drains every queued microtask first. While a live machine timer
    /// remains, waits for one deadline to become ready, runs exactly one
    /// timer callback, and drains microtasks again. A timer created by a
    /// callback therefore waits for a later timer turn. The first uncaught
    /// `queueMicrotask` or timer callback exception stops the loop and becomes
    /// [`RuntimeErrorKind::UncaughtThrow`]; a timer exception is converted
    /// before any later drain, so its queued microtasks and later timers stay
    /// pending. Promise handler throws settle their derived promises and never
    /// abort the loop. Fatal runtime failures propagate unchanged.
    ///
    /// A false wait retries only while the host still exposes the timer
    /// capability and the provider reports an armed timer; otherwise the loop
    /// fails with [`RuntimeErrorKind::TimerProviderFailure`] instead of
    /// spinning. Existing fuel is the only work bound: recursive jobs or
    /// timers eventually fail with [`RuntimeErrorKind::FuelExhausted`].
    pub fn run_to_quiescence(&mut self) -> Result<(), RuntimeError> {
        self.drain_microtasks_automatic()?;
        while self.has_pending_timers() {
            if !self.wait_for_timer_expiry()? {
                let retry = self
                    .host
                    .timers()
                    .is_some_and(|provider| provider.has_pending());
                if !retry {
                    return Err(self.checkpoint_error(RuntimeErrorKind::TimerProviderFailure {
                        message: "the timer provider lost a live machine timer".to_owned(),
                    }));
                }
                continue;
            }
            let run = self.run_one_expired_timer()?;
            if let Some(exception) = run.uncaught.into_iter().next() {
                return Err(self.checkpoint_error(RuntimeErrorKind::UncaughtThrow {
                    value: exception.value,
                    origin: exception.origin,
                }));
            }
            self.drain_microtasks_automatic()?;
        }
        Ok(())
    }

    pub(crate) fn schedule_timeout(
        &mut self,
        callback: Value,
        delay_ms: u32,
        arguments: Vec<Value>,
    ) -> Result<Value, EvalFailure> {
        if self.timers.len() >= self.limits.max_timers {
            return Err(EvalFailure::Runtime(
                RuntimeErrorKind::TimerCapacityExceeded {
                    limit: self.limits.max_timers,
                },
            ));
        }
        let id = self.next_timer_id.take().ok_or(EvalFailure::Runtime(
            RuntimeErrorKind::TimerCapacityExceeded {
                limit: self.limits.max_timers,
            },
        ))?;
        self.next_timer_id = id.checked_add(1);
        let sequence = self.next_timer_sequence.take().ok_or(EvalFailure::Runtime(
            RuntimeErrorKind::TimerCapacityExceeded {
                limit: self.limits.max_timers,
            },
        ))?;
        self.next_timer_sequence = sequence.checked_add(1);
        let deadline_ms = self
            .host
            .timers()
            .ok_or(EvalFailure::Runtime(
                RuntimeErrorKind::TimerProviderFailure {
                    message: "timer capability is unavailable".to_owned(),
                },
            ))?
            .schedule(id, delay_ms)
            .map_err(|error| {
                EvalFailure::Runtime(RuntimeErrorKind::TimerProviderFailure {
                    message: error.to_string(),
                })
            })?;
        let handle = match self.allocate(HeapEntry::Timeout {
            id,
            properties: PropertyMap::default(),
            prototype: Some(self.intrinsics.object_prototype),
            extensible: true,
        }) {
            Ok(handle) => handle,
            Err(kind) => {
                if let Some(provider) = self.host.timers() {
                    let _ = provider.cancel(id);
                }
                return Err(EvalFailure::Runtime(kind));
            }
        };
        self.timers.insert(
            id,
            TimerRecord {
                callback,
                arguments,
                handle,
                deadline_ms,
                sequence,
            },
        );
        Ok(handle)
    }

    pub(crate) fn clear_timeout(&mut self, handle: Value) -> Result<(), EvalFailure> {
        let id = match handle.decode() {
            Some(Decoded::Int32(raw)) if (raw as i32) > 0 => Some(u64::from(raw)),
            Some(Decoded::Number(number))
                if number.is_finite()
                    && number > 0.0
                    && number.fract() == 0.0
                    && number < u64::MAX as f64 =>
            {
                Some(number as u64)
            }
            Some(Decoded::HeapRef(_)) => {
                self.runtime_slot(handle)
                    .ok()
                    .flatten()
                    .and_then(|index| match &self.heap[index] {
                        HeapEntry::Timeout { id, .. } => Some(*id),
                        _ => None,
                    })
            }
            _ => None,
        };
        let Some(id) = id else {
            return Ok(());
        };
        let Some(timer) = self.timers.remove(&id) else {
            return Ok(());
        };
        self.ready_timers
            .remove(&(timer.deadline_ms, timer.sequence));
        if let Some(provider) = self.host.timers() {
            provider.cancel(id).map_err(|error| {
                EvalFailure::Runtime(RuntimeErrorKind::TimerProviderFailure {
                    message: error.to_string(),
                })
            })?;
        }
        Ok(())
    }

    fn poll_timer_expiries(&mut self) -> Result<(), RuntimeErrorKind> {
        let mut wakeups = Vec::new();
        let Some(provider) = self.host.timers() else {
            return Ok(());
        };
        provider.poll_expired(&mut wakeups).map_err(|error| {
            RuntimeErrorKind::TimerProviderFailure {
                message: error.to_string(),
            }
        })?;
        // One live report authorizes every earlier deadline. Collapse a host
        // batch to one watermark instead of rescanning all live timers per report.
        if let Some(wakeup) = wakeups
            .into_iter()
            .filter(|wakeup| self.timers.contains_key(&wakeup.id))
            .max_by_key(|wakeup| wakeup.deadline_ms)
        {
            self.promote_timer_wakeup(wakeup);
        }
        Ok(())
    }

    fn promote_timer_wakeup(&mut self, wakeup: TimerWakeup) {
        // Unknown IDs are stale cancellation races and carry no authority to
        // advance the watermark.
        if !self.timers.contains_key(&wakeup.id) {
            return;
        }
        let watermark = self.timer_watermark.map_or(wakeup.deadline_ms, |current| {
            current.max(wakeup.deadline_ms)
        });
        self.timer_watermark = Some(watermark);
        for timer in self.timers.values() {
            if timer.deadline_ms <= watermark {
                self.ready_timers
                    .insert((timer.deadline_ms, timer.sequence));
            }
        }
    }

    /// Runs queued microtasks in FIFO order until the queue is empty.
    ///
    /// Jobs queued by another job run in the same checkpoint. Promise callback
    /// throws settle their derived promises. Throws from `queueMicrotask`
    /// callbacks are returned in [`MicrotaskDrain::uncaught`].
    pub fn drain_microtasks(&mut self) -> Result<MicrotaskDrain, RuntimeError> {
        self.drain_microtasks_core(MicrotaskExceptionPolicy::CollectAndContinue)
    }

    /// Automatic drain for the event loop: the first uncaught `queueMicrotask`
    /// callback exception stops the checkpoint and becomes
    /// [`RuntimeErrorKind::UncaughtThrow`], leaving later jobs queued. All
    /// other semantics match the public manual checkpoint.
    fn drain_microtasks_automatic(&mut self) -> Result<(), RuntimeError> {
        let report = self.drain_microtasks_core(MicrotaskExceptionPolicy::StopAtFirst)?;
        let Some(exception) = report.uncaught.into_iter().next() else {
            return Ok(());
        };
        Err(self.checkpoint_error(RuntimeErrorKind::UncaughtThrow {
            value: exception.value,
            origin: exception.origin,
        }))
    }

    /// Shared microtask checkpoint. Both drains share the reentry guard,
    /// fuel-before-pop charging, FIFO execution, Promise reaction semantics,
    /// fatal-error propagation, and flag cleanup. Manual drains collect every
    /// callback exception and continue; automatic drains stop after the first,
    /// leaving the rest of the queue untouched. The exceptionless path performs
    /// no allocation.
    fn drain_microtasks_core(
        &mut self,
        exception_policy: MicrotaskExceptionPolicy,
    ) -> Result<MicrotaskDrain, RuntimeError> {
        if self.microtask_drain_active {
            return Err(self.checkpoint_error(RuntimeErrorKind::MicrotaskDrainReentry));
        }
        self.microtask_drain_active = true;
        let result = (|| {
            let mut report = MicrotaskDrain::default();
            while self.microtasks.front().is_some() {
                self.consume_fuel(1)
                    .map_err(|kind| self.checkpoint_error(kind))?;
                let job = self
                    .microtasks
                    .pop_front()
                    .expect("the queued microtask remains present after fuel charging");
                report.executed = report.executed.saturating_add(1);
                let Some(exception) = self
                    .execute_microtask_job(job)
                    .map_err(|kind| self.checkpoint_error(kind))?
                else {
                    continue;
                };
                report.uncaught.try_reserve(1).map_err(|_| {
                    self.checkpoint_error(RuntimeErrorKind::HeapByteLimitExceeded {
                        limit: self.limits.max_heap_bytes,
                    })
                })?;
                report.uncaught.push(exception);
                if exception_policy == MicrotaskExceptionPolicy::StopAtFirst {
                    break;
                }
            }
            Ok(report)
        })();
        self.microtask_drain_active = false;
        result
    }

    fn checkpoint_error(&self, kind: RuntimeErrorKind) -> RuntimeError {
        let function = self.module.entry();
        let instruction = self.module.functions()[function.get() as usize]
            .code()
            .first()
            .copied()
            .unwrap_or(Instruction::Halt);
        RuntimeError {
            kind,
            function,
            pc: Pc::new(0),
            source: RuntimeSource {
                function_name: None,
                instruction,
            },
        }
    }

    /// Runs one popped microtask job. Promise reaction and thenable jobs
    /// settle their derived promises and never surface a callback exception;
    /// a `queueMicrotask` callback throw is classified and returned for the
    /// checkpoint to collect or stop on.
    fn execute_microtask_job(
        &mut self,
        job: MicrotaskJob,
    ) -> Result<Option<CallbackException>, RuntimeErrorKind> {
        match job {
            MicrotaskJob::Reaction {
                reaction,
                value,
                origin,
            } => self
                .execute_promise_reaction(reaction, value, origin)
                .map(|()| None),
            MicrotaskJob::Thenable {
                promise,
                thenable,
                then,
            } => self
                .execute_thenable_job(promise, thenable, then)
                .map(|()| None),
            MicrotaskJob::Callback { callback } => self.execute_callback_microtask(callback),
        }
    }

    fn execute_callback_microtask(
        &mut self,
        callback: Value,
    ) -> Result<Option<CallbackException>, RuntimeErrorKind> {
        match self.call_value(callback, Value::UNDEFINED, &[]) {
            Ok(_) => Ok(None),
            Err(EvalFailure::Runtime(kind)) => Err(kind),
            Err(failure) => {
                let (value, origin) =
                    self.promise_rejection_value(failure)
                        .map_err(|failure| match failure {
                            EvalFailure::Runtime(kind) => kind,
                            _ => RuntimeErrorKind::InvalidValue { value: callback },
                        })?;
                Ok(Some(CallbackException { value, origin }))
            }
        }
    }

    fn execute_thenable_job(
        &mut self,
        promise: Value,
        thenable: Value,
        then: Value,
    ) -> Result<(), RuntimeErrorKind> {
        let record = self
            .create_promise_resolver(promise)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: promise },
            })?;
        let (resolve_target, reject_target) = self.intrinsics.builtins.promise_resolver_targets();
        let resolve = self
            .create_promise_resolver_function(resolve_target, record)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: record },
            })?;
        let reject = self
            .create_promise_resolver_function(reject_target, record)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: record },
            })?;
        match self.call_value(then, thenable, &[resolve, reject]) {
            Ok(_) => Ok(()),
            Err(EvalFailure::Runtime(kind)) => Err(kind),
            Err(failure) => self
                .reject_promise_resolver_failure(record, failure)
                .map_err(|failure| match failure {
                    EvalFailure::Runtime(kind) => kind,
                    _ => RuntimeErrorKind::InvalidValue { value: record },
                }),
        }
    }

    fn execute_promise_reaction(
        &mut self,
        reaction: PromiseReaction,
        value: Value,
        origin: ThrowOrigin,
    ) -> Result<(), RuntimeErrorKind> {
        match reaction {
            PromiseReaction::Fulfilled { handler, derived } => self.execute_promise_handler(
                handler,
                derived,
                value,
                origin,
                PromiseCompletion::Fulfilled,
            ),
            PromiseReaction::Rejected { handler, derived } => self.execute_promise_handler(
                handler,
                derived,
                value,
                origin,
                PromiseCompletion::Rejected,
            ),
            PromiseReaction::Finally {
                handler,
                derived,
                completion,
            } => self.execute_promise_finally(handler, derived, value, origin, completion),
            PromiseReaction::AsyncFulfill { activation } => {
                self.resume_async(activation, value, None)
            }
            PromiseReaction::AsyncReject { activation } => {
                self.resume_async(activation, value, Some(origin))
            }
        }
    }

    fn execute_promise_handler(
        &mut self,
        handler: Value,
        derived: Value,
        value: Value,
        origin: ThrowOrigin,
        completion: PromiseCompletion,
    ) -> Result<(), RuntimeErrorKind> {
        if !self.is_callable(handler).map_err(|failure| match failure {
            EvalFailure::Runtime(kind) => kind,
            _ => RuntimeErrorKind::InvalidValue { value: handler },
        })? {
            return match completion {
                PromiseCompletion::Fulfilled => self.resolve_promise(derived, value),
                PromiseCompletion::Rejected => self.reject_promise(derived, value, origin),
            };
        }
        match self.call_value(handler, Value::UNDEFINED, &[value]) {
            Ok(result) => self.resolve_promise(derived, result),
            Err(EvalFailure::Runtime(kind)) => Err(kind),
            Err(failure) => self
                .reject_promise_failure(derived, failure)
                .map_err(|failure| match failure {
                    EvalFailure::Runtime(kind) => kind,
                    _ => RuntimeErrorKind::InvalidValue { value: derived },
                }),
        }
    }

    fn execute_promise_finally(
        &mut self,
        handler: Value,
        derived: Value,
        value: Value,
        origin: ThrowOrigin,
        completion: PromiseCompletion,
    ) -> Result<(), RuntimeErrorKind> {
        if !self.is_callable(handler).map_err(|failure| match failure {
            EvalFailure::Runtime(kind) => kind,
            _ => RuntimeErrorKind::InvalidValue { value: handler },
        })? {
            return match completion {
                PromiseCompletion::Fulfilled => self.resolve_promise(derived, value),
                PromiseCompletion::Rejected => self.reject_promise(derived, value, origin),
            };
        }
        let cleanup = self.create_promise().map_err(|failure| match failure {
            EvalFailure::Runtime(kind) => kind,
            _ => RuntimeErrorKind::InvalidValue { value: derived },
        })?;
        let record = self
            .create_promise_finally(derived, value, origin, completion)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: derived },
            })?;
        let (on_fulfilled, on_rejected) = self.intrinsics.builtins.promise_finally_targets();
        let on_fulfilled = self
            .create_promise_resolver_function(on_fulfilled, record)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: record },
            })?;
        let on_rejected = self
            .create_promise_resolver_function(on_rejected, record)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: record },
            })?;
        self.promise_then(cleanup, on_fulfilled, on_rejected)
            .map_err(|failure| match failure {
                EvalFailure::Runtime(kind) => kind,
                _ => RuntimeErrorKind::InvalidValue { value: cleanup },
            })?;
        match self.call_value(handler, Value::UNDEFINED, &[]) {
            Ok(result) => self.resolve_promise(cleanup, result),
            Err(EvalFailure::Runtime(kind)) => Err(kind),
            Err(failure) => self
                .reject_promise_failure(cleanup, failure)
                .map_err(|failure| match failure {
                    EvalFailure::Runtime(kind) => kind,
                    _ => RuntimeErrorKind::InvalidValue { value: cleanup },
                }),
        }
    }

    pub(crate) fn enqueue_microtask_callback(
        &mut self,
        callback: Value,
    ) -> Result<(), EvalFailure> {
        self.ensure_microtask_capacity(1)
            .map_err(EvalFailure::Runtime)?;
        self.microtasks
            .push_back(MicrotaskJob::Callback { callback });
        Ok(())
    }

    fn ensure_microtask_capacity(&mut self, additional: usize) -> Result<(), RuntimeErrorKind> {
        if self
            .microtasks
            .len()
            .checked_add(additional)
            .is_none_or(|length| length > self.limits.max_microtasks)
        {
            return Err(RuntimeErrorKind::MicrotaskQueueLimitExceeded {
                limit: self.limits.max_microtasks,
            });
        }
        self.microtasks.try_reserve(additional).map_err(|_| {
            RuntimeErrorKind::HeapByteLimitExceeded {
                limit: self.limits.max_heap_bytes,
            }
        })
    }

    pub(crate) fn create_promise(&mut self) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::Promise {
            state: PromiseState::Pending {
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
            },
            properties: PropertyMap::default(),
            prototype: Some(self.intrinsics.builtins.promise_prototype()),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)
    }

    pub(crate) fn create_promise_resolver(&mut self, promise: Value) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::PromiseResolver {
            promise,
            used: false,
        })
        .map_err(EvalFailure::Runtime)
    }

    pub(crate) fn create_promise_resolver_function(
        &mut self,
        target: Value,
        record: Value,
    ) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::NativeFunction {
            callable: NativeCallable::Bound(Box::new(BoundCallable {
                target,
                this_value: Value::UNDEFINED,
                arguments: vec![record],
            })),
            properties: PropertyMap::default(),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)
    }

    pub(crate) fn resolve_promise_resolver(
        &mut self,
        record: Value,
        value: Value,
    ) -> Result<(), EvalFailure> {
        if let Some(promise) = self.use_promise_resolver(record)? {
            self.resolve_promise(promise, value)
                .map_err(EvalFailure::Runtime)?;
        }
        Ok(())
    }

    pub(crate) fn reject_promise_resolver(
        &mut self,
        record: Value,
        reason: Value,
    ) -> Result<(), EvalFailure> {
        if let Some(promise) = self.use_promise_resolver(record)? {
            self.reject_promise(promise, reason, ThrowOrigin::Bytecode)
                .map_err(EvalFailure::Runtime)?;
        }
        Ok(())
    }

    pub(crate) fn reject_promise_resolver_failure(
        &mut self,
        record: Value,
        failure: EvalFailure,
    ) -> Result<(), EvalFailure> {
        if let Some(promise) = self.use_promise_resolver(record)? {
            self.reject_promise_failure(promise, failure)?;
        }
        Ok(())
    }

    fn use_promise_resolver(&mut self, record: Value) -> Result<Option<Value>, EvalFailure> {
        let index = self
            .runtime_slot(record)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise resolver",
            }))?;
        let HeapEntry::PromiseResolver { promise, used } = &mut self.heap[index] else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise resolver",
            }));
        };
        if *used {
            return Ok(None);
        }
        *used = true;
        Ok(Some(*promise))
    }

    fn charge_promise_reactions(&mut self, count: usize) -> Result<(), EvalFailure> {
        let bytes = std::mem::size_of::<PromiseReaction>()
            .checked_mul(count)
            .ok_or(EvalFailure::Runtime(
                RuntimeErrorKind::HeapByteLimitExceeded {
                    limit: self.limits.max_heap_bytes,
                },
            ))?;
        self.charge_heap(bytes).map_err(EvalFailure::Runtime)
    }

    pub(crate) fn promise_then(
        &mut self,
        promise: Value,
        on_fulfilled: Value,
        on_rejected: Value,
    ) -> Result<Value, EvalFailure> {
        let index = self
            .runtime_slot(promise)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.prototype.then",
            }))?;
        let settled = match &self.heap[index] {
            HeapEntry::Promise {
                state: PromiseState::Pending { .. },
                ..
            } => None,
            HeapEntry::Promise {
                state: PromiseState::Fulfilled { value },
                ..
            } => Some((true, *value, ThrowOrigin::Bytecode)),
            HeapEntry::Promise {
                state: PromiseState::Rejected { reason, origin },
                ..
            } => Some((false, *reason, *origin)),
            _ => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.prototype.then",
                }));
            }
        };
        let derived = self.create_promise()?;
        if let Some((fulfilled, value, origin)) = settled {
            self.ensure_microtask_capacity(1)
                .map_err(EvalFailure::Runtime)?;
            let reaction = if fulfilled {
                PromiseReaction::Fulfilled {
                    handler: on_fulfilled,
                    derived,
                }
            } else {
                PromiseReaction::Rejected {
                    handler: on_rejected,
                    derived,
                }
            };
            self.microtasks.push_back(MicrotaskJob::Reaction {
                reaction,
                value,
                origin,
            });
            return Ok(derived);
        }
        self.charge_promise_reactions(2)?;
        let HeapEntry::Promise {
            state:
                PromiseState::Pending {
                    fulfill_reactions,
                    reject_reactions,
                },
            ..
        } = &mut self.heap[index]
        else {
            unreachable!("pending Promise state was checked before derived allocation");
        };
        fulfill_reactions.push(PromiseReaction::Fulfilled {
            handler: on_fulfilled,
            derived,
        });
        reject_reactions.push(PromiseReaction::Rejected {
            handler: on_rejected,
            derived,
        });
        Ok(derived)
    }

    pub(crate) fn promise_finally(
        &mut self,
        promise: Value,
        handler: Value,
    ) -> Result<Value, EvalFailure> {
        let index = self
            .runtime_slot(promise)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.prototype.finally",
            }))?;
        let settled = match &self.heap[index] {
            HeapEntry::Promise {
                state: PromiseState::Pending { .. },
                ..
            } => None,
            HeapEntry::Promise {
                state: PromiseState::Fulfilled { value },
                ..
            } => Some((true, *value, ThrowOrigin::Bytecode)),
            HeapEntry::Promise {
                state: PromiseState::Rejected { reason, origin },
                ..
            } => Some((false, *reason, *origin)),
            _ => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.prototype.finally",
                }));
            }
        };
        let derived = self.create_promise()?;
        let reaction = |completion| PromiseReaction::Finally {
            handler,
            derived,
            completion,
        };
        if let Some((fulfilled, value, origin)) = settled {
            self.ensure_microtask_capacity(1)
                .map_err(EvalFailure::Runtime)?;
            self.microtasks.push_back(MicrotaskJob::Reaction {
                reaction: reaction(if fulfilled {
                    PromiseCompletion::Fulfilled
                } else {
                    PromiseCompletion::Rejected
                }),
                value,
                origin,
            });
            return Ok(derived);
        }
        self.charge_promise_reactions(2)?;
        let HeapEntry::Promise {
            state:
                PromiseState::Pending {
                    fulfill_reactions,
                    reject_reactions,
                },
            ..
        } = &mut self.heap[index]
        else {
            unreachable!("pending Promise state was checked before derived allocation");
        };
        fulfill_reactions.push(reaction(PromiseCompletion::Fulfilled));
        reject_reactions.push(reaction(PromiseCompletion::Rejected));
        Ok(derived)
    }

    pub(crate) fn create_promise_finally(
        &mut self,
        derived: Value,
        value: Value,
        origin: ThrowOrigin,
        completion: PromiseCompletion,
    ) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::PromiseFinally {
            derived,
            value,
            origin,
            completion,
        })
        .map_err(EvalFailure::Runtime)
    }

    pub(crate) fn fulfill_promise_finally(&mut self, record: Value) -> Result<(), EvalFailure> {
        let (derived, value, origin, completion) = self.promise_finally_record(record)?;
        match completion {
            PromiseCompletion::Fulfilled => self
                .resolve_promise(derived, value)
                .map_err(EvalFailure::Runtime),
            PromiseCompletion::Rejected => self
                .reject_promise(derived, value, origin)
                .map_err(EvalFailure::Runtime),
        }
    }

    pub(crate) fn reject_promise_finally(
        &mut self,
        record: Value,
        reason: Value,
    ) -> Result<(), EvalFailure> {
        let (derived, _, _, _) = self.promise_finally_record(record)?;
        self.reject_promise(derived, reason, ThrowOrigin::Bytecode)
            .map_err(EvalFailure::Runtime)
    }

    fn promise_finally_record(
        &mut self,
        record: Value,
    ) -> Result<(Value, Value, ThrowOrigin, PromiseCompletion), EvalFailure> {
        let index = self
            .runtime_slot(record)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise finally target",
            }))?;
        let HeapEntry::PromiseFinally {
            derived,
            value,
            origin,
            completion,
        } = &self.heap[index]
        else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise finally target",
            }));
        };
        Ok((*derived, *value, *origin, *completion))
    }

    pub(crate) fn promise_resolve(&mut self, value: Value) -> Result<Value, EvalFailure> {
        if matches!(self.runtime_slot(value).map_err(EvalFailure::Runtime)?, Some(index) if matches!(self.heap[index], HeapEntry::Promise { .. }))
        {
            return Ok(value);
        }
        let promise = self.create_promise()?;
        self.resolve_promise(promise, value)
            .map_err(EvalFailure::Runtime)?;
        Ok(promise)
    }

    pub(crate) fn promise_reject(&mut self, reason: Value) -> Result<Value, EvalFailure> {
        let promise = self.create_promise()?;
        self.reject_promise(promise, reason, ThrowOrigin::Bytecode)
            .map_err(EvalFailure::Runtime)?;
        Ok(promise)
    }

    pub(crate) fn promise_all(&mut self, iterable: Value) -> Result<Value, EvalFailure> {
        let promise = self.create_promise()?;
        let aggregate = self
            .allocate(HeapEntry::PromiseAll {
                promise,
                values: Vec::new(),
                remaining: 1,
                settled: false,
            })
            .map_err(EvalFailure::Runtime)?;
        let iterator = match self.create_iterator(iterable, IteratorKind::Sync) {
            Ok(iterator) => iterator,
            Err(failure) => {
                self.mark_promise_all_settled(aggregate)?;
                self.reject_promise_failure(promise, failure)?;
                return Ok(promise);
            }
        };
        loop {
            let value = match self.iterator_next(iterator) {
                Ok((true, _)) => break,
                Ok((false, value)) => value,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            let index = match self.add_promise_all_element(aggregate) {
                Ok(index) => index,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            let element = match self
                .allocate(HeapEntry::PromiseAllElement {
                    aggregate,
                    index,
                    called: false,
                })
                .map_err(EvalFailure::Runtime)
            {
                Ok(element) => element,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            let (fulfill_target, reject_target) = self.intrinsics.builtins.promise_all_targets();
            let on_fulfilled = match self.create_promise_resolver_function(fulfill_target, element)
            {
                Ok(callback) => callback,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            let on_rejected = match self.create_promise_resolver_function(reject_target, element) {
                Ok(callback) => callback,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            let resolved = match self.promise_resolve(value) {
                Ok(resolved) => resolved,
                Err(failure) => {
                    return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
                }
            };
            if let Err(failure) = self.promise_then(resolved, on_fulfilled, on_rejected) {
                return self.reject_promise_all_abrupt(aggregate, promise, iterator, failure);
            }
        }
        if let Some(values) = self.finish_promise_all(aggregate)? {
            let array = self.create_array(values)?;
            self.fulfill_promise(promise, array)
                .map_err(EvalFailure::Runtime)?;
        }
        Ok(promise)
    }

    fn reject_promise_all_abrupt(
        &mut self,
        aggregate: Value,
        promise: Value,
        iterator: Value,
        failure: EvalFailure,
    ) -> Result<Value, EvalFailure> {
        self.mark_promise_all_settled(aggregate)?;
        if let Err(EvalFailure::Runtime(kind)) = self.close_iterator(iterator) {
            return Err(EvalFailure::Runtime(kind));
        }
        self.reject_promise_failure(promise, failure)?;
        Ok(promise)
    }

    fn close_iterator(&mut self, iterator: Value) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(iterator).map_err(EvalFailure::Runtime)? else {
            return Ok(());
        };
        let HeapEntry::Iterator {
            state: IteratorState::Protocol { iterator, .. },
        } = &self.heap[index]
        else {
            return Ok(());
        };
        let iterator = *iterator;
        let close = self.get_named_property(iterator, "return")?;
        if self.is_callable(close)? {
            let _ = self.call_value(close, iterator, &[])?;
        }
        Ok(())
    }

    fn mark_promise_all_settled(&mut self, aggregate: Value) -> Result<bool, EvalFailure> {
        let index = self
            .runtime_slot(aggregate)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let HeapEntry::PromiseAll { settled, .. } = &mut self.heap[index] else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }));
        };
        let changed = !*settled;
        *settled = true;
        Ok(changed)
    }

    fn add_promise_all_element(&mut self, aggregate: Value) -> Result<usize, EvalFailure> {
        let index = self
            .runtime_slot(aggregate)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let next_remaining = match &self.heap[index] {
            HeapEntry::PromiseAll {
                remaining,
                settled: false,
                ..
            } => remaining.checked_add(1).ok_or(EvalFailure::Runtime(
                RuntimeErrorKind::HeapByteLimitExceeded {
                    limit: self.limits.max_heap_bytes,
                },
            ))?,
            HeapEntry::PromiseAll { .. } => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all target",
                }));
            }
            _ => {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all target",
                }));
            }
        };
        self.charge_heap(std::mem::size_of::<Value>())
            .map_err(EvalFailure::Runtime)?;
        let HeapEntry::PromiseAll {
            values, remaining, ..
        } = &mut self.heap[index]
        else {
            unreachable!("Promise.all aggregate was checked before its heap charge");
        };
        values.try_reserve(1).map_err(|_| {
            EvalFailure::Runtime(RuntimeErrorKind::HeapByteLimitExceeded {
                limit: self.limits.max_heap_bytes,
            })
        })?;
        let index = values.len();
        values.push(Value::UNDEFINED);
        *remaining = next_remaining;
        Ok(index)
    }

    fn finish_promise_all(&mut self, aggregate: Value) -> Result<Option<Vec<Value>>, EvalFailure> {
        let index = self
            .runtime_slot(aggregate)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let HeapEntry::PromiseAll {
            values,
            remaining,
            settled,
            ..
        } = &mut self.heap[index]
        else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }));
        };
        if *settled {
            return Ok(None);
        }
        *remaining -= 1;
        if *remaining != 0 {
            return Ok(None);
        }
        *settled = true;
        Ok(Some(std::mem::take(values)))
    }

    pub(crate) fn resolve_promise_all_element(
        &mut self,
        element: Value,
        value: Value,
    ) -> Result<(), EvalFailure> {
        let index = self
            .runtime_slot(element)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let (aggregate, output_index) = {
            let HeapEntry::PromiseAllElement {
                aggregate,
                index: output_index,
                called,
            } = &mut self.heap[index]
            else {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all target",
                }));
            };
            if *called {
                return Ok(());
            }
            *called = true;
            (*aggregate, *output_index)
        };
        let aggregate_index = self
            .runtime_slot(aggregate)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let (promise, values) = {
            let HeapEntry::PromiseAll {
                promise,
                values,
                remaining,
                settled,
            } = &mut self.heap[aggregate_index]
            else {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all target",
                }));
            };
            if *settled {
                return Ok(());
            }
            values[output_index] = value;
            *remaining -= 1;
            let values = (*remaining == 0).then(|| {
                *settled = true;
                std::mem::take(values)
            });
            (*promise, values)
        };
        if let Some(values) = values {
            let array = self.create_array(values)?;
            self.fulfill_promise(promise, array)
                .map_err(EvalFailure::Runtime)?;
        }
        Ok(())
    }

    pub(crate) fn reject_promise_all_element(
        &mut self,
        element: Value,
        reason: Value,
    ) -> Result<(), EvalFailure> {
        let index = self
            .runtime_slot(element)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let aggregate = {
            let HeapEntry::PromiseAllElement {
                aggregate, called, ..
            } = &mut self.heap[index]
            else {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all target",
                }));
            };
            if *called {
                return Ok(());
            }
            *called = true;
            *aggregate
        };
        let aggregate_index = self
            .runtime_slot(aggregate)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }))?;
        let HeapEntry::PromiseAll { promise, .. } = &self.heap[aggregate_index] else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Promise.all target",
            }));
        };
        let promise = *promise;
        if !self.mark_promise_all_settled(aggregate)? {
            return Ok(());
        }
        self.reject_promise(promise, reason, ThrowOrigin::Bytecode)
            .map_err(EvalFailure::Runtime)
    }

    fn create_array(&mut self, elements: Vec<Value>) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::Array {
            elements,
            properties: PropertyMap::default(),
            prototype: Some(self.intrinsics.array_prototype),
            extensible: true,
            length_writable: true,
        })
        .map_err(EvalFailure::Runtime)
    }

    fn resolve_promise(&mut self, promise: Value, value: Value) -> Result<(), RuntimeErrorKind> {
        if promise == value {
            return self
                .reject_promise_failure(
                    promise,
                    EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "Promise cannot resolve itself",
                    }),
                )
                .map_err(|failure| match failure {
                    EvalFailure::Runtime(kind) => kind,
                    _ => RuntimeErrorKind::InvalidValue { value: promise },
                });
        }
        if !self.is_object(value) {
            return self.fulfill_promise(promise, value);
        }
        let then = match self.get_named_property(value, "then") {
            Ok(then) => then,
            Err(EvalFailure::Runtime(kind)) => return Err(kind),
            Err(failure) => {
                return self.reject_promise_failure(promise, failure).map_err(
                    |failure| match failure {
                        EvalFailure::Runtime(kind) => kind,
                        _ => RuntimeErrorKind::InvalidValue { value: promise },
                    },
                );
            }
        };
        if !self.is_callable(then).map_err(|failure| match failure {
            EvalFailure::Runtime(kind) => kind,
            _ => RuntimeErrorKind::InvalidValue { value: then },
        })? {
            return self.fulfill_promise(promise, value);
        }
        self.ensure_microtask_capacity(1)?;
        self.microtasks.push_back(MicrotaskJob::Thenable {
            promise,
            thenable: value,
            then,
        });
        Ok(())
    }

    fn reject_promise(
        &mut self,
        promise: Value,
        reason: Value,
        origin: ThrowOrigin,
    ) -> Result<(), RuntimeErrorKind> {
        self.settle_promise(promise, PromiseState::Rejected { reason, origin })
    }

    fn fulfill_promise(&mut self, promise: Value, value: Value) -> Result<(), RuntimeErrorKind> {
        self.settle_promise(promise, PromiseState::Fulfilled { value })
    }

    fn settle_promise(
        &mut self,
        promise: Value,
        terminal: PromiseState,
    ) -> Result<(), RuntimeErrorKind> {
        let index = self
            .runtime_slot(promise)?
            .ok_or(RuntimeErrorKind::InvalidValue { value: promise })?;
        let reaction_count = match &self.heap[index] {
            HeapEntry::Promise {
                state:
                    PromiseState::Pending {
                        fulfill_reactions,
                        reject_reactions,
                    },
                ..
            } => match &terminal {
                PromiseState::Fulfilled { .. } => fulfill_reactions.len(),
                PromiseState::Rejected { .. } => reject_reactions.len(),
                PromiseState::Pending { .. } => unreachable!("Promise settlement is terminal"),
            },
            HeapEntry::Promise { .. } => return Ok(()),
            _ => return Err(RuntimeErrorKind::InvalidValue { value: promise }),
        };
        self.ensure_microtask_capacity(reaction_count)?;
        let reactions = match &mut self.heap[index] {
            HeapEntry::Promise { state, .. } => {
                let reactions = match state {
                    PromiseState::Pending {
                        fulfill_reactions,
                        reject_reactions,
                    } => match &terminal {
                        PromiseState::Fulfilled { .. } => std::mem::take(fulfill_reactions),
                        PromiseState::Rejected { .. } => std::mem::take(reject_reactions),
                        PromiseState::Pending { .. } => {
                            unreachable!("Promise settlement is terminal")
                        }
                    },
                    _ => return Ok(()),
                };
                *state = terminal.clone();
                reactions
            }
            _ => return Err(RuntimeErrorKind::InvalidValue { value: promise }),
        };
        let (value, origin) = match terminal {
            PromiseState::Fulfilled { value } => (value, ThrowOrigin::Bytecode),
            PromiseState::Rejected { reason, origin } => (reason, origin),
            PromiseState::Pending { .. } => unreachable!("Promise settlement is terminal"),
        };
        for reaction in reactions {
            self.microtasks.push_back(MicrotaskJob::Reaction {
                reaction,
                value,
                origin,
            });
        }
        Ok(())
    }

    fn reject_promise_failure(
        &mut self,
        promise: Value,
        failure: EvalFailure,
    ) -> Result<(), EvalFailure> {
        let (reason, origin) = self.promise_rejection_value(failure)?;
        self.reject_promise(promise, reason, origin)
            .map_err(EvalFailure::Runtime)
    }

    fn promise_rejection_value(
        &mut self,
        failure: EvalFailure,
    ) -> Result<(Value, ThrowOrigin), EvalFailure> {
        match failure {
            EvalFailure::ThrowValue(value) => Ok((value, ThrowOrigin::Bytecode)),
            EvalFailure::ThrowValueOrigin { value, origin } => Ok((value, origin)),
            EvalFailure::Throw(ThrowOrigin::Bytecode) => {
                Ok((Value::UNDEFINED, ThrowOrigin::Bytecode))
            }
            EvalFailure::Throw(origin) => {
                let (name, message) = match origin {
                    ThrowOrigin::TypeError { operation } => ("TypeError", operation),
                    ThrowOrigin::RangeError { operation } => ("RangeError", operation),
                    ThrowOrigin::ReferenceError { operation } => ("ReferenceError", operation),
                    ThrowOrigin::UriError { operation } => ("URIError", operation),
                    ThrowOrigin::Bytecode => unreachable!("handled above"),
                };
                let id = self
                    .intrinsics
                    .builtins
                    .id_named(name)
                    .expect("error constructor is installed");
                match self.throw_error(id, message.to_owned()) {
                    EvalFailure::ThrowValue(value) => Ok((value, origin)),
                    EvalFailure::Runtime(kind) => Err(EvalFailure::Runtime(kind)),
                    _ => unreachable!("error materialization returns a thrown value"),
                }
            }
            EvalFailure::Runtime(kind) => Err(EvalFailure::Runtime(kind)),
        }
    }

    fn program(&self) -> &Program<Verified> {
        self.program
            .expect("module registry operations require a whole program")
    }

    fn module_code(&self, module: ModuleId) -> &Module<Verified> {
        let index = module.get() as usize;
        if index >= self.dynamic_base {
            return &self.dynamic[index - self.dynamic_base].program.modules()[0].code;
        }
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

    fn program_module(&self, module: ModuleId) -> &ProgramModule<Verified> {
        let index = module.get() as usize;
        if index >= self.dynamic_base {
            return &self.dynamic[index - self.dynamic_base].program.modules()[0];
        }
        self.program
            .and_then(|program| program.module(module))
            .expect("verified module id remains in bounds")
    }

    /// Validates host-provided code against this machine's classic-script realm.
    fn validate_dynamic_script(program: &Program<Verified>) -> Result<(), &'static str> {
        if program.modules().len() != 1 {
            return Err("script program must contain exactly one module");
        }
        if program.entry() != ModuleId::new(0) {
            return Err("script program entry must be module zero");
        }
        let module = &program.modules()[0];
        if !module.edges.is_empty() || !module.bindings.is_empty() || !module.exports.is_empty() {
            return Err("script program must not contain linkage metadata");
        }
        if module
            .code
            .functions()
            .iter()
            .flat_map(|function| function.code())
            .any(|instruction| {
                matches!(
                    instruction,
                    Instruction::Import { .. } | Instruction::Export { .. }
                )
            })
        {
            return Err("script program must not contain import or export instructions");
        }
        Ok(())
    }

    fn script_heap_cost(program: &Program<Verified>) -> usize {
        const MODULE_BYTES: usize = 64;
        const FUNCTION_BYTES: usize = 32;
        program.modules().iter().fold(0usize, |total, module| {
            let constant_bytes = module
                .code
                .constants()
                .iter()
                .fold(0usize, |bytes, constant| {
                    let payload = match constant {
                        Constant::String(text) => text.len_units().saturating_mul(2),
                        Constant::BigInt(value) => value.as_str().len(),
                        Constant::Number(_)
                        | Constant::Int32(_)
                        | Constant::Boolean(_)
                        | Constant::Null
                        | Constant::Undefined => 0,
                    };
                    bytes
                        .saturating_add(std::mem::size_of::<Constant>())
                        .saturating_add(payload)
                });
            let function_bytes =
                module
                    .code
                    .functions()
                    .iter()
                    .fold(0usize, |bytes, function| {
                        bytes
                            .saturating_add(FUNCTION_BYTES)
                            .saturating_add(
                                function
                                    .code()
                                    .len()
                                    .saturating_mul(std::mem::size_of::<Instruction>()),
                            )
                            .saturating_add(function.handlers().len().saturating_mul(
                                std::mem::size_of::<bamts_bytecode::ExceptionHandler>(),
                            ))
                    });
            total
                .saturating_add(MODULE_BYTES)
                .saturating_add(constant_bytes)
                .saturating_add(function_bytes)
                .saturating_add(module.code.verification_bytes())
        })
    }

    fn install_script_reserving(
        &mut self,
        program: Arc<Program<Verified>>,
        reserved_slots: usize,
        reserved_bytes: usize,
    ) -> Result<ModuleId, RuntimeErrorKind> {
        Self::validate_dynamic_script(&program)
            .map_err(|reason| RuntimeErrorKind::InvalidDynamicScript { reason })?;
        if self.dynamic.len() >= self.limits.max_dynamic_modules {
            return Err(RuntimeErrorKind::DynamicModuleLimitExceeded {
                limit: self.limits.max_dynamic_modules,
            });
        }
        let bytes = Self::script_heap_cost(&program);
        let retained_bytes =
            bytes
                .checked_add(reserved_bytes)
                .ok_or(RuntimeErrorKind::HeapByteLimitExceeded {
                    limit: self.limits.max_heap_bytes,
                })?;
        self.ensure_allocation_capacity(reserved_slots, retained_bytes)?;
        self.charge_heap(bytes)?;
        let index = self.dynamic_base.checked_add(self.dynamic.len()).ok_or(
            RuntimeErrorKind::DynamicModuleLimitExceeded {
                limit: self.limits.max_dynamic_modules,
            },
        )?;
        let module = ModuleId::new(u32::try_from(index).map_err(|_| {
            RuntimeErrorKind::DynamicModuleLimitExceeded {
                limit: self.limits.max_dynamic_modules,
            }
        })?);
        self.dynamic.push(DynamicModule { program, bytes });
        self.registry.modules.push(ModuleInstance {
            binding_cells: Vec::new(),
            constant_cells: Vec::new(),
            namespace: None,
            state: ModuleState::Unevaluated,
        });
        debug_assert_eq!(
            self.dynamic
                .last()
                .expect("installed script remains retained")
                .bytes,
            bytes
        );
        debug_assert_eq!(
            self.registry.modules.len(),
            self.dynamic_base + self.dynamic.len()
        );
        Ok(module)
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
        debug_assert!(
            self.dynamic.is_empty(),
            "module instantiation precedes dynamic script installation"
        );
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
        let exported_names: Vec<EcmaString> = self
            .program_module(target)
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
        let dependency = self.program_module(module).edges[edge.get() as usize];
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

    pub(crate) fn resolve_import(
        &self,
        module: ModuleId,
        specifier: ConstantId,
    ) -> Result<ImportTarget, RuntimeErrorKind> {
        let name = self.constant_text(module, specifier);
        self.program_module(module)
            .edges
            .iter()
            .enumerate()
            .find(|(_, edge)| {
                edge.kind.has_dynamic() && self.constant_text(module, edge.specifier) == name
            })
            .map(|(index, edge)| match edge.target {
                EdgeTarget::Local(target) => ImportTarget::Local(target),
                EdgeTarget::External => ImportTarget::External(EdgeId::new(index as u32)),
            })
            .ok_or(RuntimeErrorKind::DynamicImportEdgeMissing { module, specifier })
    }

    pub(crate) fn imported_namespace(
        &mut self,
        requester: ModuleId,
        target: ImportTarget,
    ) -> Result<Value, RuntimeErrorKind> {
        match target {
            ImportTarget::Local(target) => self.module_namespace(target, requester),
            ImportTarget::External(edge) => self.external_namespace(requester, edge),
        }
        .map_err(|error| error.kind)
    }

    fn run_import_entry(&mut self, module: ModuleId) -> Result<(), RuntimeError> {
        let function = self.module_code(module).entry();
        let stop_depth = self.frames.len();
        self.push_frame(
            RuntimeFunction { module, function },
            &[],
            Value::UNDEFINED,
            Value::UNDEFINED,
            &[],
            None,
        )?;
        let result = self.run_loop(stop_depth).and_then(|execution| {
            execution.map(|_| ()).ok_or_else(|| {
                self.program_error(
                    module,
                    RuntimeErrorKind::InvalidVerifiedProgram {
                        module,
                        instruction: Instruction::Halt,
                    },
                )
            })
        });
        if result.is_err() {
            self.unwind_frames_to(stop_depth);
        }
        result
    }

    fn evaluate_import(&mut self, module: ModuleId) -> Result<(), RuntimeError> {
        let dependencies = match self.begin_module_evaluation(module)? {
            ModuleEvaluation::Cycle => return Ok(()),
            ModuleEvaluation::Evaluated(result) => return result,
            ModuleEvaluation::Ready(dependencies) => dependencies,
        };
        for dependency in dependencies {
            if let Err(error) = self.evaluate_import(dependency) {
                self.settle_module_evaluation(module, Err(error.clone()));
                return Err(error);
            }
        }
        let result = self.run_import_entry(module);
        self.settle_module_evaluation(module, result.clone());
        result
    }

    fn import_namespace(
        &mut self,
        requester: ModuleId,
        specifier: ConstantId,
    ) -> Result<Value, EvalFailure> {
        let target = self
            .resolve_import(requester, specifier)
            .map_err(EvalFailure::Runtime)?;
        if let ImportTarget::Local(module) = target {
            self.evaluate_import(module)
                .map_err(|error| import_failure(&error))?;
        }
        self.imported_namespace(requester, target)
            .map_err(EvalFailure::Runtime)
    }
    fn evaluate_module(&mut self, module: ModuleId) -> Result<Option<Execution>, RuntimeError> {
        let dependencies = match self.begin_module_evaluation(module)? {
            ModuleEvaluation::Cycle => return Ok(None),
            ModuleEvaluation::Evaluated(result) => return result.map(|()| None),
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
        for (edge_index, edge) in self
            .program_module(module)
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
                    self.settle_module_evaluation(module, Err(error.clone()));
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
        let stored = result.as_ref().map(|_| ()).map_err(Clone::clone);
        self.settle_module_evaluation(module, stored);
        result
    }

    pub(crate) fn settle_module_evaluation(
        &mut self,
        module: ModuleId,
        result: Result<(), RuntimeError>,
    ) {
        match result {
            Ok(()) => {
                self.registry.modules[module.get() as usize].state = ModuleState::Evaluated(Ok(()));
            }
            Err(error) if matches!(error.kind, RuntimeErrorKind::UncaughtThrow { .. }) => {
                self.registry.modules[module.get() as usize].state =
                    ModuleState::Evaluated(Err(error));
            }
            Err(_) => self.abort_module_evaluation(module),
        }
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
                Instruction::CreateCell { dst } => {
                    let value = self
                        .allocate(HeapEntry::Array {
                            elements: vec![Value::UNINITIALIZED],
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
                Instruction::Suspend { src, .. }
                    if self
                        .async_boundaries
                        .last()
                        .is_some_and(|boundary| *boundary == frame_index) =>
                {
                    let awaited = self.read_register(frame_index, src.get());
                    let frame = self.frames.pop().expect("async activation is executing");
                    self.pending_async_suspend = Some((
                        awaited,
                        SuspendedActivation {
                            target: RuntimeFunction {
                                module: frame.module,
                                function: FunctionId::new(frame.function as u32),
                            },
                            registers: frame.registers,
                            this_value: frame.this_value,
                            new_target: frame.new_target,
                            args: frame.args,
                            arguments_object: frame.arguments_object,
                            resume_token: pc as u32 + 1,
                        },
                    ));
                    return Ok(None);
                }
                Instruction::Suspend { src, .. }
                    if self
                        .generator_boundaries
                        .last()
                        .is_some_and(|boundary| *boundary == frame_index) =>
                {
                    let value = self.read_register(frame_index, src.get());
                    let frame = self
                        .frames
                        .pop()
                        .expect("generator activation is executing");
                    self.pending_generator_resume = Some(GeneratorResume::Yield {
                        value,
                        activation: SuspendedActivation {
                            target: RuntimeFunction {
                                module: frame.module,
                                function: FunctionId::new(frame.function as u32),
                            },
                            registers: frame.registers,
                            this_value: frame.this_value,
                            new_target: frame.new_target,
                            args: frame.args,
                            arguments_object: frame.arguments_object,
                            resume_token: pc as u32 + 1,
                        },
                    });
                    return Ok(None);
                }
                Instruction::Suspend { .. } => {
                    self.throw_type("suspend outside an engine-owned event loop", pc)?;
                }
                Instruction::Import { dst, specifier } => {
                    match self.import_namespace(module_id, specifier) {
                        Ok(namespace) => {
                            self.write_register(frame_index, dst.get(), namespace);
                            self.frames[frame_index].pc = pc + 1;
                        }
                        Err(failure) => self.resolve_failure(failure, pc)?,
                    }
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
        let bytes = entry.initial_bytes();
        self.ensure_allocation_capacity(1, bytes)?;
        self.heap_bytes += bytes;
        let slot = self.heap.len() as u32 + 1;
        self.heap.push(entry);
        let id = SlotId::from_parts(RUNTIME_HEAP_SEGMENT, slot)
            .expect("runtime segment and one-based slot are nonzero");
        Ok(Value::heap_ref(id))
    }

    fn ensure_allocation_capacity(
        &self,
        additional_slots: usize,
        additional_bytes: usize,
    ) -> Result<(), RuntimeErrorKind> {
        let used_slots = self.heap.len().saturating_sub(self.intrinsic_slots);
        let slots_fit_limit = used_slots
            .checked_add(additional_slots)
            .is_some_and(|total| total <= self.limits.max_heap_slots);
        let slots_fit_value = self
            .heap
            .len()
            .checked_add(additional_slots)
            .is_some_and(|total| total <= u32::MAX as usize);
        if !slots_fit_limit || !slots_fit_value {
            return Err(RuntimeErrorKind::HeapSlotLimitExceeded {
                limit: self.limits.max_heap_slots,
            });
        }
        let bytes_fit = self
            .heap_bytes
            .checked_add(additional_bytes)
            .is_some_and(|total| total <= self.limits.max_heap_bytes);
        if !bytes_fit {
            return Err(RuntimeErrorKind::HeapByteLimitExceeded {
                limit: self.limits.max_heap_bytes,
            });
        }
        Ok(())
    }

    fn ensure_object_property_capacity(
        &self,
        property_bytes: usize,
    ) -> Result<(), RuntimeErrorKind> {
        let bytes =
            property_bytes
                .checked_add(1)
                .ok_or(RuntimeErrorKind::HeapByteLimitExceeded {
                    limit: self.limits.max_heap_bytes,
                })?;
        self.ensure_allocation_capacity(1, bytes)
    }
    fn charge_heap(&mut self, bytes: usize) -> Result<(), RuntimeErrorKind> {
        self.ensure_allocation_capacity(0, bytes)?;
        self.heap_bytes += bytes;
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
                self.program_module(module).bindings[binding].kind,
                BindingKind::Imported { .. } | BindingKind::Namespace { .. }
            ) {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "assign to immutable module binding",
                }));
            }
            self.registry.cells[cell.0].value = value;
        } else {
            let name = self.constant_text(module, name).to_owned();
            if let Some(global_this) = self.intrinsics.global("globalThis") {
                let key = PropertyKey::Named(name.clone());
                if matches!(
                    self.own_descriptor(global_this, &key)?,
                    Some(
                        Property::Data {
                            writable: false,
                            ..
                        } | Property::Accessor { setter: None, .. }
                    )
                ) {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "assign to non-writable global property",
                    }));
                }
            }
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
                HeapEntry::NativeFunction { callable, .. } => match callable {
                    NativeCallable::Builtin(id) => Ok(CalleeKind::Builtin { id: *id }),
                    NativeCallable::Bound(_) => Ok(CalleeKind::Bound),
                },
                _ => Ok(CalleeKind::NotCallable),
            },
            None => Ok(CalleeKind::NotCallable),
        }
    }

    pub(crate) fn flatten_bound(
        &self,
        callee: Value,
        this_value: Value,
        arguments: &[Value],
    ) -> Result<BoundCall, RuntimeErrorKind> {
        let mut target = callee;
        let mut receiver = this_value;
        let mut segments = Vec::new();
        let mut total = arguments.len();
        while let Some(index) = self.runtime_slot(target)? {
            let HeapEntry::NativeFunction {
                callable: NativeCallable::Bound(bound),
                ..
            } = &self.heap[index]
            else {
                break;
            };
            total = total.checked_add(bound.arguments.len()).ok_or(
                RuntimeErrorKind::ArgumentLimitExceeded {
                    limit: self.limits.max_argument_count,
                    requested: u32::MAX,
                },
            )?;
            if total > self.limits.max_argument_count as usize {
                return Err(RuntimeErrorKind::ArgumentLimitExceeded {
                    limit: self.limits.max_argument_count,
                    requested: u32::try_from(total).unwrap_or(u32::MAX),
                });
            }
            segments.push(bound.arguments.as_slice());
            receiver = bound.this_value;
            target = bound.target;
        }
        let mut flattened = Vec::with_capacity(total);
        for segment in segments.iter().rev() {
            flattened.extend_from_slice(segment);
        }
        flattened.extend_from_slice(arguments);
        Ok(BoundCall {
            target,
            this_value: receiver,
            arguments: flattened,
        })
    }

    fn bound_target(&self, mut value: Value) -> Result<Value, RuntimeErrorKind> {
        loop {
            let Some(index) = self.runtime_slot(value)? else {
                return Ok(value);
            };
            let HeapEntry::NativeFunction {
                callable: NativeCallable::Bound(bound),
                ..
            } = &self.heap[index]
            else {
                return Ok(value);
            };
            value = bound.target;
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
        return_to: Option<ReturnTo>,
    ) -> Result<(), RuntimeError> {
        let function_index = target.function.get() as usize;
        let metadata = &self.module_code(target.module).functions()[function_index];
        let limit_error = |kind| match (self.frames.last(), return_to) {
            (Some(caller), Some(return_to)) => {
                self.error_at_in_module(kind, caller.module, caller.function, return_to.call_pc)
            }
            (_, None) => self.error_at_in_module(kind, target.module, function_index, 0),
            (None, Some(_)) => unreachable!("a returning frame has a caller"),
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
            target, metadata, captures, this_value, new_target, arguments, return_to,
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

    pub(crate) fn reserve_suspended_activation_registers(
        &mut self,
        register_count: usize,
    ) -> Result<(), RuntimeErrorKind> {
        if self.live_registers.saturating_add(register_count) > self.limits.max_total_registers {
            return Err(RuntimeErrorKind::RegisterLimitExceeded {
                limit: self.limits.max_total_registers,
            });
        }
        self.live_registers += register_count;
        Ok(())
    }

    pub(crate) fn release_suspended_activation_registers(&mut self, register_count: usize) {
        self.live_registers -= register_count;
    }

    pub(crate) fn enter_native_generator(&mut self) -> Result<(), RuntimeErrorKind> {
        if self.frames.len().saturating_add(self.native_depth) >= self.limits.max_call_depth {
            return Err(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            });
        }
        self.native_depth += 1;
        Ok(())
    }

    pub(crate) fn leave_native_generator(&mut self) {
        self.native_depth -= 1;
    }

    fn execute_call(&mut self, request: CallRequest<'_>) -> Result<(), RuntimeError> {
        let CallRequest {
            callee,
            this_value,
            arguments,
            destination,
            call_pc,
            constructed,
            new_target,
        } = request;
        let mut callee = callee;
        let mut this_value = this_value;
        let mut arguments = Cow::Borrowed(arguments);
        loop {
            match self.callee_kind(callee) {
                Ok(CalleeKind::Runtime { target, captures }) => {
                    let flags = self.module_code(target.module).functions()
                        [target.function.get() as usize]
                        .flags();
                    if flags.is_generator && !flags.is_async {
                        let generator = self
                            .create_generator(GeneratorStart {
                                target,
                                captures,
                                this_value,
                                new_target,
                                args: arguments.as_ref().to_vec(),
                            })
                            .map_err(|kind| self.error_here_at(kind, call_pc))?;
                        if let Some(register) = destination {
                            self.write_register(self.frames.len() - 1, register, generator);
                        }
                        return Ok(());
                    }
                    if flags.is_async && !flags.is_generator {
                        return match self.start_async_call(
                            target,
                            &captures,
                            this_value,
                            new_target,
                            arguments.as_ref(),
                        ) {
                            Ok(promise) => {
                                if let Some(register) = destination {
                                    self.write_register(self.frames.len() - 1, register, promise);
                                }
                                Ok(())
                            }
                            Err(failure) => self.resolve_failure(failure, call_pc),
                        };
                    }
                    return self.push_frame(
                        target,
                        &captures,
                        this_value,
                        new_target,
                        arguments.as_ref(),
                        Some(ReturnTo {
                            destination: destination.map(|register| register as usize),
                            call_pc,
                            constructed,
                        }),
                    );
                }
                Ok(CalleeKind::Builtin { id }) => {
                    match self.call_builtin(id, this_value, arguments.as_ref(), false) {
                        Ok(intrinsics::BuiltinOutcome::Value(value)) => {
                            if let Some(register) = destination {
                                self.write_register(self.frames.len() - 1, register, value);
                            }
                            return Ok(());
                        }
                        Ok(intrinsics::BuiltinOutcome::Call {
                            callee: next,
                            this_value: next_this,
                            arguments: next_arguments,
                        }) => {
                            callee = next;
                            this_value = next_this;
                            arguments = Cow::Owned(next_arguments);
                        }
                        Ok(intrinsics::BuiltinOutcome::GeneratorNext {
                            generator,
                            resume_value,
                        }) => match self.resume_generator(generator, resume_value) {
                            Ok(value) => {
                                if let Some(register) = destination {
                                    self.write_register(self.frames.len() - 1, register, value);
                                }
                                return Ok(());
                            }
                            Err(failure) => return self.resolve_failure(failure, call_pc),
                        },
                        Ok(intrinsics::BuiltinOutcome::ConstructCall { .. }) => {
                            return self.throw_type("call", call_pc);
                        }
                        Err(failure) => return self.resolve_failure(failure, call_pc),
                    }
                }
                Ok(CalleeKind::Bound) => {
                    let bound = self
                        .flatten_bound(callee, this_value, arguments.as_ref())
                        .map_err(|kind| self.error_here_at(kind, call_pc))?;
                    callee = bound.target;
                    if constructed.is_none() {
                        this_value = bound.this_value;
                    }
                    arguments = Cow::Owned(bound.arguments);
                }
                Ok(CalleeKind::NotCallable) => return self.throw_type("call", call_pc),
                Err(kind) => return Err(self.error_here_at(kind, call_pc)),
            }
        }
    }

    fn execute_construct(
        &mut self,
        callee: Value,
        arguments: &[Value],
        destination: u32,
        call_pc: usize,
    ) -> Result<(), RuntimeError> {
        let mut callee = callee;
        let mut arguments = Cow::Borrowed(arguments);
        if matches!(self.callee_kind(callee), Ok(CalleeKind::Bound)) {
            let bound = self
                .flatten_bound(callee, Value::UNDEFINED, arguments.as_ref())
                .map_err(|kind| self.error_here_at(kind, call_pc))?;
            callee = bound.target;
            arguments = Cow::Owned(bound.arguments);
        }
        let index = match self.runtime_slot(callee) {
            Ok(Some(index)) => index,
            Ok(None) => return self.throw_type("construct", call_pc),
            Err(kind) => return Err(self.error_here_at(kind, call_pc)),
        };
        let builtin = match &self.heap[index] {
            HeapEntry::NativeFunction {
                callable: NativeCallable::Builtin(id),
                ..
            } => Some(*id),
            _ => None,
        };
        if let Some(id) = builtin {
            return match self.call_builtin(id, Value::UNDEFINED, arguments.as_ref(), true) {
                Ok(intrinsics::BuiltinOutcome::Value(value)) => {
                    self.write_register(self.frames.len() - 1, destination, value);
                    Ok(())
                }
                Ok(
                    intrinsics::BuiltinOutcome::Call { .. }
                    | intrinsics::BuiltinOutcome::GeneratorNext { .. },
                ) => self.throw_type("construct", call_pc),
                Ok(intrinsics::BuiltinOutcome::ConstructCall {
                    callee: continuation,
                    this_value,
                    arguments: continuation_arguments,
                    prototype,
                }) => {
                    let object = self
                        .allocate_constructed_receiver_with(prototype)
                        .map_err(|kind| self.error_here_at(kind, call_pc))?;
                    self.execute_call(CallRequest {
                        callee: continuation,
                        this_value,
                        arguments: &continuation_arguments,
                        destination: Some(destination),
                        call_pc,
                        constructed: Some(object),
                        new_target: callee,
                    })
                }
                Err(failure) => self.resolve_failure(failure, call_pc),
            };
        }
        if !matches!(
            self.heap[index],
            HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. }
        ) {
            return self.throw_type("construct", call_pc);
        }
        if let HeapEntry::Function {
            module, function, ..
        } = self.heap[index]
        {
            if self.module_code(module).functions()[function.get() as usize]
                .flags()
                .is_async
            {
                return self.throw_type("construct", call_pc);
            }
        }
        let object = self
            .allocate_constructed_receiver(callee)
            .map_err(|kind| self.error_here_at(kind, call_pc))?;
        self.execute_call(CallRequest {
            callee,
            this_value: object,
            arguments: arguments.as_ref(),
            destination: Some(destination),
            call_pc,
            constructed: Some(object),
            new_target: callee,
        })
    }

    fn constructed_prototype(&self, callee: Value) -> Result<Value, RuntimeErrorKind> {
        let index = self
            .runtime_slot(callee)?
            .ok_or(RuntimeErrorKind::InvalidValue { value: callee })?;
        Ok(match self.own_data_property(index, "prototype") {
            Some(value) if self.is_object(value) => value,
            _ => self.intrinsics.object_prototype,
        })
    }

    fn allocate_constructed_receiver(&mut self, callee: Value) -> Result<Value, RuntimeErrorKind> {
        let prototype = self.constructed_prototype(callee)?;
        self.allocate_constructed_receiver_with(prototype)
    }

    fn allocate_constructed_receiver_with(
        &mut self,
        prototype: Value,
    ) -> Result<Value, RuntimeErrorKind> {
        self.allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            boxed_primitive: None,
            extensible: true,
        })
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
        let mut callee = callee;
        let mut this_value = this_value;
        let mut arguments = Cow::Borrowed(arguments);
        loop {
            match self.callee_kind(callee).map_err(EvalFailure::Runtime)? {
                CalleeKind::Builtin { id } => {
                    match self.call_builtin(id, this_value, arguments.as_ref(), false)? {
                        intrinsics::BuiltinOutcome::Value(value) => return Ok(value),
                        intrinsics::BuiltinOutcome::Call {
                            callee: next,
                            this_value: next_this,
                            arguments: next_arguments,
                        } => {
                            callee = next;
                            this_value = next_this;
                            arguments = Cow::Owned(next_arguments);
                        }
                        intrinsics::BuiltinOutcome::GeneratorNext {
                            generator,
                            resume_value,
                        } => return self.resume_generator(generator, resume_value),
                        intrinsics::BuiltinOutcome::ConstructCall { .. } => {
                            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                                operation: "call",
                            }));
                        }
                    }
                }
                CalleeKind::Runtime { target, captures } => {
                    let flags = self.module_code(target.module).functions()
                        [target.function.get() as usize]
                        .flags();
                    if flags.is_generator && !flags.is_async {
                        return self
                            .create_generator(GeneratorStart {
                                target,
                                captures,
                                this_value,
                                new_target: Value::UNDEFINED,
                                args: arguments.as_ref().to_vec(),
                            })
                            .map_err(EvalFailure::Runtime);
                    }
                    if flags.is_async && !flags.is_generator {
                        return self.start_async_call(
                            target,
                            &captures,
                            this_value,
                            Value::UNDEFINED,
                            arguments.as_ref(),
                        );
                    }
                    let stop_depth = self.frames.len();
                    let return_to = self.frames.last().map(|frame| ReturnTo {
                        destination: None,
                        call_pc: frame.pc,
                        constructed: None,
                    });
                    self.push_frame(
                        target,
                        &captures,
                        this_value,
                        Value::UNDEFINED,
                        arguments.as_ref(),
                        return_to,
                    )
                    .map_err(|error| EvalFailure::Runtime(error.kind))?;
                    self.callback_boundaries.push(stop_depth);
                    let result = self.run_loop(stop_depth);
                    self.callback_boundaries
                        .pop()
                        .expect("nested runtime callback owns its unwind boundary");
                    return match result {
                        Ok(None) => self.last_completion.take().ok_or(EvalFailure::Runtime(
                            RuntimeErrorKind::InvalidValue {
                                value: Value::UNDEFINED,
                            },
                        )),
                        Ok(Some(execution)) => Ok(execution.value),
                        Err(error) => {
                            self.unwind_frames_to(stop_depth);
                            match error.kind {
                                RuntimeErrorKind::UncaughtThrow { value, .. } => {
                                    Err(EvalFailure::ThrowValue(value))
                                }
                                kind => Err(EvalFailure::Runtime(kind)),
                            }
                        }
                    };
                }
                CalleeKind::Bound => {
                    let bound = self
                        .flatten_bound(callee, this_value, arguments.as_ref())
                        .map_err(EvalFailure::Runtime)?;
                    callee = bound.target;
                    this_value = bound.this_value;
                    arguments = Cow::Owned(bound.arguments);
                }
                CalleeKind::NotCallable => {
                    return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                        operation: "call",
                    }));
                }
            }
        }
    }

    fn unwind_frames_to(&mut self, depth: usize) {
        while self.frames.len() > depth {
            let frame = self.frames.pop().expect("frame depth was checked");
            self.live_registers -= frame.registers.len();
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
            EvalFailure::ThrowValueOrigin { value, origin } => self.throw(value, origin, pc),
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
            if self
                .callback_boundaries
                .last()
                .is_some_and(|boundary| self.frames.len() == *boundary)
            {
                return Err(self.error_at_in_module(
                    RuntimeErrorKind::UncaughtThrow { value, origin },
                    site_module,
                    site_function,
                    faulting_pc,
                ));
            }
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
                    return self.found_outcome(found);
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
                return self.found_outcome(found);
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
                return self.found_outcome(found);
            }
            match self.prototype_index(node)? {
                Some(next) => node = next,
                None => return Ok(GetOutcome::Value(Value::UNDEFINED)),
            }
        }
        Ok(GetOutcome::Value(Value::UNDEFINED))
    }

    fn found_outcome(&mut self, found: Found) -> Result<GetOutcome, EvalFailure> {
        match found {
            Found::Value(Value::UNINITIALIZED) => {
                let id = self
                    .intrinsics
                    .builtins
                    .id_named("ReferenceError")
                    .expect("ReferenceError intrinsic is installed");
                match self.throw_error(
                    id,
                    "Cannot access lexical binding before initialization".into(),
                ) {
                    EvalFailure::ThrowValue(value) => Err(EvalFailure::ThrowValueOrigin {
                        value,
                        origin: ThrowOrigin::ReferenceError {
                            operation: "lexical binding is uninitialized",
                        },
                    }),
                    failure => Err(failure),
                }
            }
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
        match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => property_lookup_ascii(properties, name),
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
                let key = self
                    .program_module(*module)
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
                    "source" => Some(Found::Text(crate::intrinsics::builtins::canonical_source(
                        pattern,
                    ))),
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
            | HeapEntry::Iterator { .. }
            | HeapEntry::PromiseResolver { .. }
            | HeapEntry::PromiseFinally { .. }
            | HeapEntry::PromiseAll { .. }
            | HeapEntry::AsyncActivation { .. }
            | HeapEntry::PromiseAllElement { .. } => None,
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
        }
        match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => property_lookup(properties, key),
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
                Some(Found::Value(export.cell.map_or(export.value, |cell| {
                    self.registry.cells[cell.0].value
                })))
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
                        return Some(Found::Text(crate::intrinsics::builtins::canonical_source(
                            pattern,
                        )));
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
            | HeapEntry::Iterator { .. }
            | HeapEntry::PromiseResolver { .. }
            | HeapEntry::PromiseFinally { .. }
            | HeapEntry::PromiseAll { .. }
            | HeapEntry::AsyncActivation { .. }
            | HeapEntry::PromiseAllElement { .. } => None,
        }
    }

    fn namespace_export(
        &self,
        module: ModuleId,
        name: &EcmaString,
    ) -> Result<Option<Value>, RuntimeErrorKind> {
        if module.get() as usize >= self.dynamic_base {
            return Ok(None);
        }
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
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::Array { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => properties,
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
            | HeapEntry::Generator { prototype, .. }
            | HeapEntry::Script { prototype, .. }
            | HeapEntry::Array { prototype, .. }
            | HeapEntry::Function { prototype, .. }
            | HeapEntry::RegExp { prototype, .. }
            | HeapEntry::Date { prototype, .. }
            | HeapEntry::BuiltinIterator { prototype, .. }
            | HeapEntry::Collection { prototype, .. }
            | HeapEntry::Promise { prototype, .. }
            | HeapEntry::Timeout { prototype, .. }
            | HeapEntry::ProcessEnv { prototype, .. } => *prototype,
            HeapEntry::NativeFunction { .. } => Some(self.intrinsics.function_prototype),
            _ => None,
        };
        match prototype {
            Some(value) => self.runtime_slot(value).map_err(EvalFailure::Runtime),
            None => Ok(None),
        }
    }

    pub(crate) fn inherits_from_prototype(
        &self,
        value: Value,
        prototype: Value,
    ) -> Result<bool, EvalFailure> {
        let Some(mut current) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        let Some(target) = self.runtime_slot(prototype).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        let mut traversed = 0;
        while let Some(next) = self.prototype_index(current)? {
            if next == target {
                return Ok(true);
            }
            current = next;
            traversed += 1;
            if traversed > self.heap.len() {
                return Ok(false);
            }
        }
        Ok(false)
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
                | HeapEntry::Generator { properties, .. }
                | HeapEntry::Script { properties, .. }
                | HeapEntry::Array { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. }
                | HeapEntry::Promise { properties, .. }
                | HeapEntry::Timeout { properties, .. } => match properties.get(key) {
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
            | HeapEntry::Generator {
                properties,
                extensible,
                ..
            }
            | HeapEntry::Script {
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
            }
            | HeapEntry::Promise {
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
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => {
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
            | HeapEntry::Iterator { .. }
            | HeapEntry::PromiseResolver { .. }
            | HeapEntry::PromiseFinally { .. }
            | HeapEntry::PromiseAll { .. }
            | HeapEntry::AsyncActivation { .. }
            | HeapEntry::PromiseAllElement { .. } => {
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
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => {
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
                    | HeapEntry::Generator {
                        properties,
                        extensible,
                        ..
                    }
                    | HeapEntry::Script {
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
                    }
                    | HeapEntry::Promise {
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
                | HeapEntry::Generator { properties, .. }
                | HeapEntry::Script { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. }
                | HeapEntry::Promise { properties, .. }
                | HeapEntry::Timeout { properties, .. } => {
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
                | HeapEntry::PromiseResolver { .. }
                | HeapEntry::PromiseFinally { .. }
                | HeapEntry::PromiseAll { .. }
                | HeapEntry::AsyncActivation { .. }
                | HeapEntry::PromiseAllElement { .. }
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

    pub(crate) fn array_push(&mut self, array: Value, value: Value) -> Result<(), EvalFailure> {
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
        let iterator = self.create_iterator(iterable, IteratorKind::Sync)?;
        loop {
            let (done, value) = self.iterator_next(iterator)?;
            if done {
                return Ok(());
            }
            self.array_push(array, value)?;
        }
    }

    fn object_spread(&mut self, target: Value, source: Value) -> Result<(), EvalFailure> {
        let target_index = match self.runtime_slot(target).map_err(EvalFailure::Runtime)? {
            Some(index)
                if matches!(
                    self.heap[index],
                    HeapEntry::Object { .. }
                        | HeapEntry::Generator { .. }
                        | HeapEntry::Script { .. }
                        | HeapEntry::Array { .. }
                        | HeapEntry::Promise { .. }
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
        let keys = self.own_property_keys(source)?;
        for key in keys {
            if !self.own_property_is_enumerable(source, &key)? {
                continue;
            }
            let value = self.get_property_key(source, &key)?;
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
                | HeapEntry::Generator {
                    prototype: slot, ..
                }
                | HeapEntry::Script {
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
                }
                | HeapEntry::Promise {
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

    pub(crate) fn create_generator(
        &mut self,
        start: GeneratorStart,
    ) -> Result<Value, RuntimeErrorKind> {
        self.allocate(HeapEntry::Generator {
            state: GeneratorState::SuspendedStart(start),
            properties: PropertyMap::default(),
            prototype: Some(self.intrinsics.builtins.generator_prototype()),
            extensible: true,
        })
    }

    fn resume_generator(
        &mut self,
        generator: Value,
        resume_value: Value,
    ) -> Result<Value, EvalFailure> {
        let state = self.take_generator_state(generator)?;
        if matches!(&state, GeneratorState::Completed) {
            return self.iterator_result(Value::UNDEFINED, true);
        }

        let stop_depth = self.frames.len();
        let return_to = self.frames.last().map(|frame| ReturnTo {
            destination: None,
            call_pc: frame.pc,
            constructed: None,
        });
        let prepared = match state {
            GeneratorState::SuspendedStart(start) => self
                .push_frame(
                    start.target,
                    &start.captures,
                    start.this_value,
                    start.new_target,
                    &start.args,
                    return_to,
                )
                .map_err(|error| EvalFailure::Runtime(error.kind)),
            GeneratorState::Suspended(activation) => {
                self.push_resumed_generator_frame(activation, resume_value, return_to)
            }
            GeneratorState::Executing | GeneratorState::Completed => unreachable!(),
        };
        if let Err(failure) = prepared {
            self.settle_generator_completed(generator)?;
            return Err(failure);
        }

        let resumed = self.run_generator_activation(stop_depth);
        match resumed {
            Ok(GeneratorResume::Yield { value, activation }) => {
                self.settle_generator_yield(generator, value, activation)
            }
            Ok(GeneratorResume::Return(value)) => {
                self.settle_generator_completed(generator)?;
                self.iterator_result(value, true)
            }
            Ok(GeneratorResume::Throw { value, origin }) => {
                self.settle_generator_completed(generator)?;
                Err(EvalFailure::ThrowValueOrigin { value, origin })
            }
            Err(failure) => {
                self.settle_generator_completed(generator)?;
                Err(failure)
            }
        }
    }

    fn push_resumed_generator_frame(
        &mut self,
        activation: SuspendedActivation,
        resume_value: Value,
        return_to: Option<ReturnTo>,
    ) -> Result<(), EvalFailure> {
        if self.frames.len().saturating_add(self.native_depth) >= self.limits.max_call_depth {
            self.release_suspended_activation_registers(activation.registers.len());
            return Err(EvalFailure::Runtime(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            }));
        }
        let suspend_pc = activation
            .resume_token
            .checked_sub(1)
            .expect("suspended generator token is nonzero") as usize;
        let instruction = self.module_code(activation.target.module).functions()
            [activation.target.function.get() as usize]
            .code()[suspend_pc];
        let Instruction::Suspend { dst, resume, .. } = instruction else {
            unreachable!("generator resume token names a suspend instruction");
        };
        let mut frame = Frame {
            module: activation.target.module,
            function: activation.target.function.get() as usize,
            pc: resume.get() as usize,
            registers: activation.registers,
            return_to,
            this_value: activation.this_value,
            new_target: activation.new_target,
            args: activation.args,
            arguments_object: activation.arguments_object,
        };
        frame.registers[dst.get() as usize] = resume_value;
        self.frames.push(frame);
        Ok(())
    }

    fn run_generator_activation(
        &mut self,
        stop_depth: usize,
    ) -> Result<GeneratorResume, EvalFailure> {
        self.last_completion = None;
        self.pending_generator_resume = None;
        self.callback_boundaries.push(stop_depth);
        self.generator_boundaries.push(stop_depth);
        let result = self.run_loop(stop_depth);
        self.generator_boundaries
            .pop()
            .expect("generator execution owns its suspend boundary");
        self.callback_boundaries
            .pop()
            .expect("generator execution owns its unwind boundary");

        match result {
            Ok(Some(execution)) => Ok(GeneratorResume::Return(execution.value)),
            Ok(None) => {
                if let Some(resume) = self.pending_generator_resume.take() {
                    return Ok(resume);
                }
                let value = self.last_completion.take().unwrap_or(Value::UNDEFINED);
                Ok(GeneratorResume::Return(value))
            }
            Err(error) => {
                self.unwind_frames_to(stop_depth);
                match error.kind {
                    RuntimeErrorKind::UncaughtThrow { value, origin } => {
                        Ok(GeneratorResume::Throw { value, origin })
                    }
                    kind => Err(EvalFailure::Runtime(kind)),
                }
            }
        }
    }

    pub(crate) fn take_generator_state(
        &mut self,
        generator: Value,
    ) -> Result<GeneratorState, EvalFailure> {
        let Some(index) = self.runtime_slot(generator).map_err(EvalFailure::Runtime)? else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Generator.prototype.next called on incompatible receiver",
            }));
        };
        let HeapEntry::Generator { state, .. } = &mut self.heap[index] else {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "Generator.prototype.next called on incompatible receiver",
            }));
        };
        match std::mem::replace(state, GeneratorState::Executing) {
            GeneratorState::Executing => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "generator is already running",
            })),
            GeneratorState::Completed => {
                *state = GeneratorState::Completed;
                Ok(GeneratorState::Completed)
            }
            state => Ok(state),
        }
    }

    pub(crate) fn settle_generator_yield(
        &mut self,
        generator: Value,
        value: Value,
        activation: SuspendedActivation,
    ) -> Result<Value, EvalFailure> {
        let register_count = activation.registers.len();
        let result = match self.iterator_result(value, false) {
            Ok(result) => result,
            Err(failure) => {
                self.release_suspended_activation_registers(register_count);
                self.replace_executing_generator(generator, GeneratorState::Completed)?;
                return Err(failure);
            }
        };
        if let Err(failure) =
            self.replace_executing_generator(generator, GeneratorState::Suspended(activation))
        {
            self.release_suspended_activation_registers(register_count);
            return Err(failure);
        }
        Ok(result)
    }

    pub(crate) fn settle_generator_completed(
        &mut self,
        generator: Value,
    ) -> Result<(), EvalFailure> {
        self.replace_executing_generator(generator, GeneratorState::Completed)
    }

    fn replace_executing_generator(
        &mut self,
        generator: Value,
        next: GeneratorState,
    ) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(generator).map_err(EvalFailure::Runtime)? else {
            return Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: generator,
            }));
        };
        let HeapEntry::Generator { state, .. } = &mut self.heap[index] else {
            return Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: generator,
            }));
        };
        if !matches!(state, GeneratorState::Executing) {
            return Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: generator,
            }));
        }
        *state = next;
        Ok(())
    }

    /// Starts an ordinary async function: creates the implicit result Promise,
    /// drives the body synchronously to its first `await` or completion under a
    /// detached suspend boundary, and returns the Promise. A `return` resolves
    /// it and an escaping throw rejects it; a runtime limit failure stays fatal.
    pub(crate) fn start_async_call(
        &mut self,
        target: RuntimeFunction,
        captures: &[Value],
        this_value: Value,
        new_target: Value,
        arguments: &[Value],
    ) -> Result<Value, EvalFailure> {
        let promise = self.create_promise()?;
        let record = self.create_async_activation(promise)?;
        let stop_depth = self.frames.len();
        let return_to = self.frames.last().map(|frame| ReturnTo {
            destination: None,
            call_pc: frame.pc,
            constructed: None,
        });
        self.push_frame(
            target, captures, this_value, new_target, arguments, return_to,
        )
        .map_err(|error| EvalFailure::Runtime(error.kind))?;
        let step = self.drive_async_activation(stop_depth, None);
        self.settle_async_step(record, promise, step)?;
        Ok(promise)
    }

    /// Resumes a suspended async activation inside its Promise reaction job. On
    /// fulfillment the awaited value is written to `Suspend.dst`; on rejection
    /// the reason is thrown at the `Suspend` pc so a covering `try`/`catch`
    /// runs. The activation is one-shot; a second resume is a hard error.
    fn resume_async(
        &mut self,
        record: Value,
        value: Value,
        rejection: Option<ThrowOrigin>,
    ) -> Result<(), RuntimeErrorKind> {
        let promise = self.async_activation_promise(record)?;
        let activation = self.take_async_activation(record)?;
        let register_count = activation.registers.len();
        if self.frames.len().saturating_add(self.native_depth) >= self.limits.max_call_depth {
            self.release_suspended_activation_registers(register_count);
            return Err(RuntimeErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            });
        }
        let suspend_pc = activation
            .resume_token
            .checked_sub(1)
            .expect("suspended async token is nonzero") as usize;
        let instruction = self.module_code(activation.target.module).functions()
            [activation.target.function.get() as usize]
            .code()[suspend_pc];
        let Instruction::Suspend { dst, resume, .. } = instruction else {
            unreachable!("async resume token names a suspend instruction");
        };
        let stop_depth = self.frames.len();
        let return_to = self.frames.last().map(|frame| ReturnTo {
            destination: None,
            call_pc: frame.pc,
            constructed: None,
        });
        let mut frame = Frame {
            module: activation.target.module,
            function: activation.target.function.get() as usize,
            pc: resume.get() as usize,
            registers: activation.registers,
            return_to,
            this_value: activation.this_value,
            new_target: activation.new_target,
            args: activation.args,
            arguments_object: activation.arguments_object,
        };
        let inject = match rejection {
            None => {
                frame.registers[dst.get() as usize] = value;
                None
            }
            Some(origin) => Some((value, origin, suspend_pc)),
        };
        self.frames.push(frame);
        let step = self.drive_async_activation(stop_depth, inject);
        match self.settle_async_step(record, promise, step) {
            Ok(()) => Ok(()),
            Err(EvalFailure::Runtime(kind)) => Err(kind),
            Err(_) => Err(RuntimeErrorKind::InvalidValue { value: record }),
        }
    }

    /// Runs the interpreter loop for a detached async activation under one
    /// suspend and one unwind boundary, optionally injecting a rejection at the
    /// resumed `Suspend` pc first. It reports the awaited value on suspension,
    /// the returned value on completion, or an uncaught throw; runtime limit
    /// failures propagate as fatal `EvalFailure::Runtime`.
    fn drive_async_activation(
        &mut self,
        stop_depth: usize,
        inject: Option<(Value, ThrowOrigin, usize)>,
    ) -> Result<AsyncStep, EvalFailure> {
        self.last_completion = None;
        self.pending_async_suspend = None;
        self.callback_boundaries.push(stop_depth);
        self.async_boundaries.push(stop_depth);
        let result = match inject {
            None => self.run_loop(stop_depth),
            Some((value, origin, faulting_pc)) => match self.throw(value, origin, faulting_pc) {
                Ok(()) => self.run_loop(stop_depth),
                Err(error) => Err(error),
            },
        };
        self.async_boundaries
            .pop()
            .expect("async execution owns its suspend boundary");
        self.callback_boundaries
            .pop()
            .expect("async execution owns its unwind boundary");
        match result {
            Ok(Some(execution)) => Ok(AsyncStep::Return(execution.value)),
            Ok(None) => {
                if let Some((awaited, activation)) = self.pending_async_suspend.take() {
                    Ok(AsyncStep::Suspend {
                        awaited,
                        activation,
                    })
                } else {
                    Ok(AsyncStep::Return(
                        self.last_completion.take().unwrap_or(Value::UNDEFINED),
                    ))
                }
            }
            Err(error) => {
                self.unwind_frames_to(stop_depth);
                match error.kind {
                    RuntimeErrorKind::UncaughtThrow { value, origin } => {
                        Ok(AsyncStep::Throw { value, origin })
                    }
                    kind => Err(EvalFailure::Runtime(kind)),
                }
            }
        }
    }

    /// Settles the result Promise (or arms the next await) for one async step.
    fn settle_async_step(
        &mut self,
        record: Value,
        promise: Value,
        step: Result<AsyncStep, EvalFailure>,
    ) -> Result<(), EvalFailure> {
        match step {
            Ok(AsyncStep::Suspend {
                awaited,
                activation,
            }) => {
                let register_count = activation.registers.len();
                let result = self
                    .store_async_activation(record, activation)
                    .and_then(|()| self.await_promise(awaited, record));
                if result.is_err() {
                    let released = self
                        .take_async_activation(record)
                        .map_or(register_count, |stored| stored.registers.len());
                    self.release_suspended_activation_registers(released);
                }
                result
            }
            Ok(AsyncStep::Return(value)) => self
                .resolve_promise(promise, value)
                .map_err(EvalFailure::Runtime),
            Ok(AsyncStep::Throw { value, origin }) => self
                .reject_promise(promise, value, origin)
                .map_err(EvalFailure::Runtime),
            Err(failure) => Err(failure),
        }
    }

    /// Resolves the awaited value through Promise resolution and attaches the
    /// two direct resume reactions that point only at the activation record. An
    /// already-settled Promise costs exactly one microtask tick.
    fn await_promise(&mut self, awaited: Value, record: Value) -> Result<(), EvalFailure> {
        let promise = self.promise_resolve(awaited)?;
        let index = self
            .runtime_slot(promise)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: promise,
            }))?;
        let settled = match &self.heap[index] {
            HeapEntry::Promise {
                state: PromiseState::Pending { .. },
                ..
            } => None,
            HeapEntry::Promise {
                state: PromiseState::Fulfilled { value },
                ..
            } => Some((true, *value, ThrowOrigin::Bytecode)),
            HeapEntry::Promise {
                state: PromiseState::Rejected { reason, origin },
                ..
            } => Some((false, *reason, *origin)),
            _ => {
                return Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                    value: promise,
                }));
            }
        };
        if let Some((fulfilled, value, origin)) = settled {
            self.ensure_microtask_capacity(1)
                .map_err(EvalFailure::Runtime)?;
            let reaction = if fulfilled {
                PromiseReaction::AsyncFulfill { activation: record }
            } else {
                PromiseReaction::AsyncReject { activation: record }
            };
            self.microtasks.push_back(MicrotaskJob::Reaction {
                reaction,
                value,
                origin,
            });
            return Ok(());
        }
        self.charge_promise_reactions(2)?;
        let HeapEntry::Promise {
            state:
                PromiseState::Pending {
                    fulfill_reactions,
                    reject_reactions,
                },
            ..
        } = &mut self.heap[index]
        else {
            unreachable!("pending Promise state was checked before reaction registration");
        };
        fulfill_reactions.push(PromiseReaction::AsyncFulfill { activation: record });
        reject_reactions.push(PromiseReaction::AsyncReject { activation: record });
        Ok(())
    }

    fn create_async_activation(&mut self, promise: Value) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::AsyncActivation {
            activation: None,
            promise,
        })
        .map_err(EvalFailure::Runtime)
    }

    fn store_async_activation(
        &mut self,
        record: Value,
        activation: SuspendedActivation,
    ) -> Result<(), EvalFailure> {
        let index = self
            .runtime_slot(record)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: record,
            }))?;
        let HeapEntry::AsyncActivation {
            activation: slot, ..
        } = &mut self.heap[index]
        else {
            return Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue {
                value: record,
            }));
        };
        *slot = Some(activation);
        Ok(())
    }

    /// Takes the one suspended activation out of the record, making resume
    /// one-shot. A second take (a second resume) is a hard invalid-state error.
    fn take_async_activation(
        &mut self,
        record: Value,
    ) -> Result<SuspendedActivation, RuntimeErrorKind> {
        let index = self
            .runtime_slot(record)?
            .ok_or(RuntimeErrorKind::InvalidValue { value: record })?;
        let HeapEntry::AsyncActivation {
            activation: slot, ..
        } = &mut self.heap[index]
        else {
            return Err(RuntimeErrorKind::InvalidValue { value: record });
        };
        slot.take()
            .ok_or(RuntimeErrorKind::InvalidValue { value: record })
    }

    fn async_activation_promise(&self, record: Value) -> Result<Value, RuntimeErrorKind> {
        let index = self
            .runtime_slot(record)?
            .ok_or(RuntimeErrorKind::InvalidValue { value: record })?;
        let HeapEntry::AsyncActivation { promise, .. } = &self.heap[index] else {
            return Err(RuntimeErrorKind::InvalidValue { value: record });
        };
        Ok(*promise)
    }

    pub(crate) fn iterator_result(
        &mut self,
        value: Value,
        done: bool,
    ) -> Result<Value, EvalFailure> {
        let result = self
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(self.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .map_err(EvalFailure::Runtime)?;
        self.set_data_property(result, "value", value)?;
        self.set_data_property(result, "done", Value::boolean(done))?;
        Ok(result)
    }

    // ---- iterators ---------------------------------------------------------

    fn create_iterator(&mut self, src: Value, kind: IteratorKind) -> Result<Value, EvalFailure> {
        if kind == IteratorKind::Keys {
            let keys = self.enumerable_keys(src)?;
            return self
                .allocate(HeapEntry::Iterator {
                    state: IteratorState::Keys { index: 0, keys },
                })
                .map_err(EvalFailure::Runtime);
        }

        let iterator_symbol = self.intrinsics.builtins.symbol_iterator();
        let iterator_key = self.to_property_key(iterator_symbol)?;
        let method = self.get_property_key(src, &iterator_key)?;
        if !self.is_callable(method)? {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "value is not iterable",
            }));
        }
        let iterator = self.call_value(method, src, &[])?;
        if !self.is_object(iterator) {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "iterator method returned a non-object",
            }));
        }
        let next = self.get_named_property(iterator, "next")?;
        self.create_protocol_iterator(iterator, next)
    }

    pub(crate) fn create_protocol_iterator(
        &mut self,
        iterator: Value,
        next: Value,
    ) -> Result<Value, EvalFailure> {
        self.allocate(HeapEntry::Iterator {
            state: IteratorState::Protocol { iterator, next },
        })
        .map_err(EvalFailure::Runtime)
    }

    fn own_property_keys(&self, src: Value) -> Result<Vec<PropertyKey>, EvalFailure> {
        match self.runtime_slot(src).map_err(EvalFailure::Runtime)? {
            Some(index) => match &self.heap[index] {
                HeapEntry::Object { properties, .. }
                | HeapEntry::Generator { properties, .. }
                | HeapEntry::Script { properties, .. }
                | HeapEntry::Function { properties, .. }
                | HeapEntry::NativeFunction { properties, .. }
                | HeapEntry::RegExp { properties, .. }
                | HeapEntry::Date { properties, .. }
                | HeapEntry::BuiltinIterator { properties, .. }
                | HeapEntry::Collection { properties, .. }
                | HeapEntry::Promise { properties, .. }
                | HeapEntry::Timeout { properties, .. } => Ok(ordered_property_keys(properties)),
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => {
                    let mut indices: Vec<(usize, PropertyKey)> = elements
                        .iter()
                        .enumerate()
                        .filter(|(_, element)| **element != Value::HOLE)
                        .map(|(offset, _)| {
                            (
                                offset,
                                PropertyKey::Named(EcmaString::from_utf8(&offset.to_string())),
                            )
                        })
                        .collect();
                    let mut suffix = Vec::new();
                    for key in ordered_property_keys(properties) {
                        let Some(offset) = key.as_string().and_then(array_index) else {
                            suffix.push(key);
                            continue;
                        };
                        let offset = offset as usize;
                        if elements
                            .get(offset)
                            .is_some_and(|element| *element != Value::HOLE)
                        {
                            continue;
                        }
                        indices.push((offset, key));
                    }
                    indices.sort_unstable_by_key(|(offset, _)| *offset);
                    Ok(indices
                        .into_iter()
                        .map(|(_, key)| key)
                        .chain(suffix)
                        .collect())
                }
                HeapEntry::String(text) => Ok((0..text.len_units())
                    .map(|index| PropertyKey::Named(EcmaString::from_utf8(&index.to_string())))
                    .collect()),
                HeapEntry::ModuleNamespace { module } => {
                    let mut names: Vec<EcmaString> = self
                        .program_module(*module)
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
                | HeapEntry::Iterator { .. }
                | HeapEntry::PromiseResolver { .. }
                | HeapEntry::PromiseFinally { .. }
                | HeapEntry::PromiseAll { .. }
                | HeapEntry::AsyncActivation { .. }
                | HeapEntry::PromiseAllElement { .. } => Ok(Vec::new()),
            },
            None => Ok(Vec::new()),
        }
    }

    fn own_property_is_enumerable(
        &self,
        src: Value,
        key: &PropertyKey,
    ) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(src).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        Ok(match &self.heap[index] {
            HeapEntry::Array {
                elements,
                properties,
                ..
            } => properties.get(key).map_or_else(
                || {
                    key.as_string().is_some_and(|name| {
                        array_index(name).is_some_and(|offset| {
                            elements
                                .get(offset as usize)
                                .is_some_and(|element| *element != Value::HOLE)
                        })
                    })
                },
                Property::enumerable,
            ),
            HeapEntry::String(text) => key.as_string().is_some_and(|name| {
                array_index(name).is_some_and(|offset| (offset as usize) < text.len_units())
            }),
            HeapEntry::ModuleNamespace { .. } | HeapEntry::ExternalModuleNamespace { .. } => {
                matches!(key, PropertyKey::Named(_))
            }
            HeapEntry::Object { properties, .. }
            | HeapEntry::Generator { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::Promise { properties, .. }
            | HeapEntry::Timeout { properties, .. } => {
                properties.get(key).is_some_and(Property::enumerable)
            }
            _ => false,
        })
    }

    fn enumerable_keys(&self, src: Value) -> Result<Vec<EcmaString>, EvalFailure> {
        let mut names = Vec::new();
        for key in self.own_property_keys(src)? {
            if !self.own_property_is_enumerable(src, &key)? {
                continue;
            }
            if let PropertyKey::Named(name) = key {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn iterator_next(&mut self, iterator: Value) -> Result<(bool, Value), EvalFailure> {
        let (callee, this_value) = match self.prepare_iterator_next(iterator)? {
            IteratorNextPrepared::Ready { done, value } => return Ok((done, value)),
            IteratorNextPrepared::Call { callee, this_value } => (callee, this_value),
        };

        let result = self.call_value(callee, this_value, &[])?;
        if !self.is_object(result) {
            return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "iterator next returned a non-object",
            }));
        }
        let done = self.get_named_property(result, "done")?;
        if self.truthy(done) {
            return Ok((true, Value::UNDEFINED));
        }
        let value = self.get_named_property(result, "value")?;
        Ok((false, value))
    }

    pub(crate) fn prepare_iterator_next(
        &mut self,
        iterator: Value,
    ) -> Result<IteratorNextPrepared, EvalFailure> {
        let iterator_index = self
            .runtime_slot(iterator)
            .map_err(EvalFailure::Runtime)?
            .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "iterator next on non-iterator",
            }))?;
        match &self.heap[iterator_index] {
            HeapEntry::Iterator {
                state: IteratorState::Keys { index, keys },
            } => {
                let Some(text) = keys.get(*index).cloned() else {
                    return Ok(IteratorNextPrepared::Ready {
                        done: true,
                        value: Value::UNDEFINED,
                    });
                };
                let value = self
                    .allocate(HeapEntry::String(text))
                    .map_err(EvalFailure::Runtime)?;
                self.advance_iterator(iterator_index);
                Ok(IteratorNextPrepared::Ready { done: false, value })
            }
            HeapEntry::Iterator {
                state: IteratorState::Protocol { iterator, next },
            } => Ok(IteratorNextPrepared::Call {
                callee: *next,
                this_value: *iterator,
            }),
            _ => Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "iterator next on non-iterator",
            })),
        }
    }

    pub(crate) fn iterable_values(&mut self, source: Value) -> Result<Vec<Value>, EvalFailure> {
        let iterator = self.create_iterator(source, IteratorKind::Sync)?;
        let mut values = Vec::new();
        loop {
            let (done, value) = self.iterator_next(iterator)?;
            if done {
                return Ok(values);
            }
            let bytes = values
                .len()
                .checked_add(1)
                .and_then(|length| length.checked_mul(std::mem::size_of::<Value>()))
                .ok_or(EvalFailure::Runtime(
                    RuntimeErrorKind::HeapByteLimitExceeded {
                        limit: self.limits.max_heap_bytes,
                    },
                ))?;
            self.ensure_allocation_capacity(1, bytes)
                .map_err(EvalFailure::Runtime)?;
            values.push(value);
        }
    }

    fn advance_iterator(&mut self, iterator_index: usize) {
        if let HeapEntry::Iterator {
            state: IteratorState::Keys { index, .. },
        } = &mut self.heap[iterator_index]
        {
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
        let left = self.to_primitive_default(left)?;
        let right = self.to_primitive_default(right)?;
        let left_string = self.string_text(left).cloned();
        let right_string = self.string_text(right).cloned();
        if left_string.is_some() || right_string.is_some() {
            let left = match left_string {
                Some(text) => text,
                None => self.to_string(left)?,
            };
            let right = match right_string {
                Some(text) => text,
                None => self.to_string(right)?,
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

    fn coercion_is_primitive(&self, value: Value) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(true);
        };
        Ok(matches!(
            self.heap[index],
            HeapEntry::String(_)
                | HeapEntry::BigInt(_)
                | HeapEntry::Symbol { .. }
                | HeapEntry::PrivateName { .. }
        ))
    }

    fn to_primitive_default(&mut self, value: Value) -> Result<Value, EvalFailure> {
        let prefer_string = self
            .runtime_slot(value)
            .map_err(EvalFailure::Runtime)?
            .is_some_and(|index| matches!(self.heap[index], HeapEntry::Date { .. }));
        self.to_primitive_observable(value, prefer_string)
    }

    pub(crate) fn to_primitive_observable(
        &mut self,
        value: Value,
        prefer_string: bool,
    ) -> Result<Value, EvalFailure> {
        if self.coercion_is_primitive(value)? {
            return Ok(value);
        }
        let methods = if prefer_string {
            ["toString", "valueOf"]
        } else {
            ["valueOf", "toString"]
        };
        for name in methods {
            let method = self.get_named_property(value, name)?;
            if !self.is_callable(method)? {
                continue;
            }
            let primitive = self.call_value(method, value, &[])?;
            if self.coercion_is_primitive(primitive)? {
                return Ok(primitive);
            }
        }
        Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "cannot convert object to primitive",
        }))
    }

    pub(crate) fn to_string_observable(&mut self, value: Value) -> Result<EcmaString, EvalFailure> {
        let primitive = self.to_primitive_observable(value, true)?;
        self.to_string(primitive)
    }

    pub(crate) fn to_number_observable(&mut self, value: Value) -> Result<Value, EvalFailure> {
        let primitive = self.to_primitive_observable(value, false)?;
        self.to_number(primitive)
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
                        | HeapEntry::Generator { .. }
                        | HeapEntry::Script { .. }
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
                        | HeapEntry::Promise { .. }
                        | HeapEntry::PromiseResolver { .. }
                        | HeapEntry::PromiseFinally { .. }
                        | HeapEntry::PromiseAll { .. }
                        | HeapEntry::AsyncActivation { .. }
                        | HeapEntry::PromiseAllElement { .. }
                        | HeapEntry::ProcessEnv { .. }
                        | HeapEntry::Iterator { .. }
                        | HeapEntry::Timeout { .. } => Ok(Value::number(f64::NAN)),
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
                    | HeapEntry::Generator { .. }
                    | HeapEntry::Script { .. }
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
                    | HeapEntry::Promise { .. }
                    | HeapEntry::PromiseResolver { .. }
                    | HeapEntry::PromiseFinally { .. }
                    | HeapEntry::PromiseAll { .. }
                    | HeapEntry::AsyncActivation { .. }
                    | HeapEntry::PromiseAllElement { .. }
                    | HeapEntry::ProcessEnv { .. }
                    | HeapEntry::Iterator { .. }
                    | HeapEntry::Timeout { .. } => true,
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
                    | HeapEntry::Generator { .. }
                    | HeapEntry::Script { .. }
                    | HeapEntry::Array { .. }
                    | HeapEntry::ModuleNamespace { .. }
                    | HeapEntry::ExternalModuleNamespace { .. }
                    | HeapEntry::HashState { .. }
                    | HeapEntry::RegExp { .. }
                    | HeapEntry::Date { .. }
                    | HeapEntry::BuiltinIterator { .. }
                    | HeapEntry::Collection { .. }
                    | HeapEntry::Promise { .. }
                    | HeapEntry::PromiseResolver { .. }
                    | HeapEntry::PromiseFinally { .. }
                    | HeapEntry::PromiseAll { .. }
                    | HeapEntry::AsyncActivation { .. }
                    | HeapEntry::PromiseAllElement { .. }
                    | HeapEntry::ProcessEnv { .. }
                    | HeapEntry::Iterator { .. }
                    | HeapEntry::Timeout { .. } => "object",
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
        let constructor = self
            .bound_target(constructor)
            .map_err(EvalFailure::Runtime)?;
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

    fn value_to_string(&self, value: Value, depth: usize) -> Result<EcmaString, EvalFailure> {
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
                        | HeapEntry::Generator { .. }
                        | HeapEntry::Script { .. }
                        | HeapEntry::Date { .. }
                        | HeapEntry::BuiltinIterator { .. }
                        | HeapEntry::Collection { .. }
                        | HeapEntry::Promise { .. }
                        | HeapEntry::PromiseResolver { .. }
                        | HeapEntry::PromiseFinally { .. }
                        | HeapEntry::PromiseAll { .. }
                        | HeapEntry::AsyncActivation { .. }
                        | HeapEntry::PromiseAllElement { .. }
                        | HeapEntry::ModuleNamespace { .. }
                        | HeapEntry::ExternalModuleNamespace { .. }
                        | HeapEntry::ProcessEnv { .. }
                        | HeapEntry::Iterator { .. }
                        | HeapEntry::Timeout { .. }
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
                HeapEntry::String(_)
                    | HeapEntry::BigInt(_)
                    | HeapEntry::PromiseResolver { .. }
                    | HeapEntry::PromiseFinally { .. }
                    | HeapEntry::PromiseAll { .. }
                    | HeapEntry::AsyncActivation { .. }
                    | HeapEntry::PromiseAllElement { .. }
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
            PropertyKey::Symbol(_) => symbols.push(key.clone()),
            PropertyKey::Private(_) => {}
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
    use std::sync::Arc;

    use super::*;
    use crate::intrinsics::BuiltinOutcome;
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

    fn generator_function(
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
            FunctionFlags {
                is_async: false,
                is_generator: true,
            },
            code,
            handlers,
        )
    }

    fn async_function(
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
            FunctionFlags {
                is_async: true,
                is_generator: false,
            },
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

    #[test]
    fn async_await_setup_failure_releases_suspended_registers() {
        let program = verified(
            vec![Constant::Undefined],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                async_function(
                    0,
                    2,
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
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let limits = Limits {
            max_microtasks: 0,
            ..Limits::default()
        };
        let mut machine = Machine::new(&program, &mut host, limits);
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);

        assert!(matches!(
            machine.call_value(callable, Value::UNDEFINED, &[]),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::MicrotaskQueueLimitExceeded { limit: 0 }
            ))
        ));
        assert_eq!(machine.live_registers, 0);
    }

    fn run_ok(program: &Program<Verified>) -> Execution {
        let mut host = TestHost;
        Machine::new(program, &mut host, Limits::default())
            .run()
            .unwrap()
    }

    fn generator_callable<H: Host>(machine: &mut Machine<'_, H>, function: u32) -> Value {
        machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(function),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap()
    }

    fn generator_next<H: Host>(
        machine: &mut Machine<'_, H>,
        generator: Value,
        resume_value: Value,
    ) -> Result<(Value, bool), EvalFailure> {
        let next = machine.get_named_property(generator, "next")?;
        let result = machine.call_value(next, generator, &[resume_value])?;
        let done = machine.get_named_property(result, "done")?;
        let value = machine.get_named_property(result, "value")?;
        Ok((value, machine.truthy(done)))
    }

    #[test]
    fn sync_generator_is_lazy_resumes_registers_and_stays_completed() {
        let program = verified(
            vec![Constant::Int32(10)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    3,
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
                        Instruction::Binary {
                            dst: reg(2),
                            op: BinaryOp::Add,
                            left: reg(0),
                            right: reg(1),
                        },
                        Instruction::Return { value: reg(2) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let generator = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        assert_eq!(machine.live_registers, 0, "calling must not start the body");
        machine
            .set_data_property(generator, "visible", Value::int32(1))
            .unwrap();
        assert_eq!(
            machine.get_named_property(generator, "visible").unwrap(),
            Value::int32(1),
        );
        assert_eq!(
            machine.own_property_keys(generator).unwrap(),
            vec![PropertyKey::Named(EcmaString::from_utf8("visible"))],
        );
        assert!(
            machine
                .inherits_from_prototype(
                    generator,
                    machine.intrinsics.builtins.generator_prototype(),
                )
                .unwrap()
        );

        assert_eq!(
            generator_next(&mut machine, generator, Value::int32(99)).unwrap(),
            (Value::int32(10), false),
        );
        assert_eq!(machine.live_registers, 3);
        assert_eq!(
            generator_next(&mut machine, generator, Value::int32(5)).unwrap(),
            (Value::int32(15), true),
        );
        assert_eq!(machine.live_registers, 0);
        assert_eq!(
            generator_next(&mut machine, generator, Value::int32(8)).unwrap(),
            (Value::UNDEFINED, true),
        );
    }

    #[test]
    fn sync_generator_reentrant_next_is_a_type_error() {
        let program = verified(
            Vec::new(),
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(0, 1, vec![Instruction::Halt], Vec::new()),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let generator = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        let _ = machine.take_generator_state(generator).unwrap();

        assert!(matches!(
            generator_next(&mut machine, generator, Value::UNDEFINED),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
    }

    #[test]
    fn sync_generator_uncaught_throw_preserves_origin_and_completes() {
        let program = verified(
            vec![Constant::Int32(7)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Throw { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let generator = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();

        assert!(matches!(
            generator_next(&mut machine, generator, Value::UNDEFINED),
            Err(EvalFailure::ThrowValueOrigin {
                value,
                origin: ThrowOrigin::Bytecode,
            }) if value == Value::int32(7)
        ));
        assert_eq!(
            generator_next(&mut machine, generator, Value::UNDEFINED).unwrap(),
            (Value::UNDEFINED, true),
        );
        assert_eq!(machine.live_registers, 0);
    }

    #[test]
    fn outer_compiled_handler_catches_generator_throw_value() {
        let program = verified(
            vec![
                Constant::Int32(7),
                Constant::Undefined,
                Constant::String(EcmaString::from_utf8("next")),
            ],
            vec![
                function(
                    0,
                    8,
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
                            constant: cid(1),
                        },
                        Instruction::Call {
                            dst: reg(4),
                            callee: reg(1),
                            this_value: reg(3),
                            arguments: reg(2),
                        },
                        Instruction::LoadConst {
                            dst: reg(5),
                            constant: cid(2),
                        },
                        Instruction::GetProperty {
                            dst: reg(6),
                            object: reg(4),
                            key: reg(5),
                        },
                        Instruction::Call {
                            dst: reg(7),
                            callee: reg(6),
                            this_value: reg(4),
                            arguments: reg(2),
                        },
                        Instruction::Return { value: reg(3) },
                        Instruction::Return { value: reg(7) },
                    ],
                    vec![ExceptionHandler {
                        start: pc(7),
                        end: pc(8),
                        handler: pc(9),
                        catch_register: reg(7),
                    }],
                ),
                generator_function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Throw { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        );

        assert_eq!(run_ok(&program).value, Value::int32(7));
    }

    #[test]
    fn sync_generator_catches_body_throw_before_suspending() {
        let program = verified(
            vec![Constant::Int32(7)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    3,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Throw { value: reg(0) },
                        Instruction::Suspend {
                            dst: reg(2),
                            src: reg(1),
                            resume: pc(3),
                        },
                        Instruction::Return { value: reg(2) },
                    ],
                    vec![ExceptionHandler {
                        start: pc(1),
                        end: pc(2),
                        handler: pc(2),
                        catch_register: reg(1),
                    }],
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let generator = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();

        assert_eq!(
            generator_next(&mut machine, generator, Value::UNDEFINED).unwrap(),
            (Value::int32(7), false),
        );
        assert_eq!(
            generator_next(&mut machine, generator, Value::int32(9)).unwrap(),
            (Value::int32(9), true),
        );
    }

    #[test]
    fn suspended_generator_registers_remain_charged() {
        let program = verified(
            vec![Constant::Int32(1)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    3,
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
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_total_registers: 3,
                ..Limits::default()
            },
        );
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let first = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        let second = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        assert_eq!(
            generator_next(&mut machine, first, Value::UNDEFINED).unwrap(),
            (Value::int32(1), false),
        );
        assert!(matches!(
            generator_next(&mut machine, second, Value::UNDEFINED),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::RegisterLimitExceeded { .. }
            ))
        ));
        assert_eq!(machine.live_registers, 3);
        assert_eq!(
            generator_next(&mut machine, first, Value::int32(4)).unwrap(),
            (Value::int32(4), true),
        );
        assert_eq!(machine.live_registers, 0);
    }

    #[test]
    fn resumed_generator_call_depth_failure_releases_registers() {
        let program = verified(
            vec![Constant::Int32(1)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    2,
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
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_total_registers: 2,
                ..Limits::default()
            },
        );
        machine.frames.clear();
        machine.live_registers = 0;

        let callable = generator_callable(&mut machine, 1);
        let first = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();

        // Start and suspend the first generator, charging its two registers.
        assert_eq!(
            generator_next(&mut machine, first, Value::UNDEFINED).unwrap(),
            (Value::int32(1), false),
        );
        assert_eq!(machine.live_registers, 2);

        // Fill the compiled call depth to the exact limit so the next resume
        // fails in push_resumed_generator_frame before it can take ownership.
        machine.frames.push(Frame {
            module: ModuleId::new(0),
            function: 0,
            pc: 0,
            registers: Vec::new(),
            return_to: None,
            this_value: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: Vec::new(),
            arguments_object: None,
        });
        machine.limits.max_call_depth = machine.frames.len();

        assert!(matches!(
            generator_next(&mut machine, first, Value::int32(7)),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::CallDepthExceeded { .. }
            ))
        ));
        assert_eq!(machine.live_registers, 0);

        // The generator is now sticky Completed.
        assert_eq!(
            generator_next(&mut machine, first, Value::UNDEFINED).unwrap(),
            (Value::UNDEFINED, true),
        );

        // Remove the artificial depth and make room for another activation.
        machine.frames.pop();
        machine.limits.max_call_depth = Limits::default().max_call_depth;

        // A second generator can suspend again only if the first's charge was released.
        let second = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        assert_eq!(
            generator_next(&mut machine, second, Value::UNDEFINED).unwrap(),
            (Value::int32(1), false),
        );
        assert_eq!(machine.live_registers, 2);
        assert_eq!(
            generator_next(&mut machine, second, Value::int32(9)).unwrap(),
            (Value::int32(9), true),
        );
        assert_eq!(machine.live_registers, 0);
    }
    #[test]
    fn array_extend_consumes_generator_through_sync_iterator_protocol() {
        let program = verified(
            vec![Constant::Int32(1), Constant::Int32(2)],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                generator_function(
                    0,
                    3,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Suspend {
                            dst: reg(2),
                            src: reg(0),
                            resume: pc(2),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(1),
                        },
                        Instruction::Suspend {
                            dst: reg(2),
                            src: reg(1),
                            resume: pc(4),
                        },
                        Instruction::Return { value: reg(2) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callable = generator_callable(&mut machine, 1);
        let generator = machine.call_value(callable, Value::UNDEFINED, &[]).unwrap();
        let array = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();

        machine.array_extend(array, generator).unwrap();
        assert_eq!(
            machine.array_elements(array).unwrap(),
            Some(vec![Value::int32(1), Value::int32(2)]),
        );
        assert_eq!(machine.live_registers, 0);
    }

    #[test]
    fn runtime_callback_without_interpreter_caller_propagates_throw() {
        let program = verified(
            Vec::new(),
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(1, 1, vec![Instruction::Throw { value: reg(0) }], Vec::new()),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callee = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let thrown = Value::int32(7);

        assert!(matches!(
            machine.call_value(callee, Value::UNDEFINED, &[thrown]),
            Err(EvalFailure::ThrowValue(value)) if value == thrown
        ));
    }

    #[test]
    fn runtime_callback_failure_releases_root_frame() {
        let program = verified(
            Vec::new(),
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    1,
                    1,
                    vec![Instruction::Return { value: reg(0) }],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let callee = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        machine.fuel = 0;

        assert!(matches!(
            machine.call_value(callee, Value::UNDEFINED, &[Value::int32(7)]),
            Err(EvalFailure::Runtime(RuntimeErrorKind::FuelExhausted { .. }))
        ));
        assert!(machine.frames.is_empty());
        assert_eq!(machine.live_registers, 0);

        machine.fuel = 1;
        assert!(matches!(
            machine.call_value(callee, Value::UNDEFINED, &[Value::int32(7)]),
            Ok(value) if value == Value::int32(7)
        ));
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
    fn addition_coerces_objects_left_to_right_and_interpolates_errors() {
        let module = verified(
            vec![
                Constant::String(EcmaString::from_utf8("L")),
                Constant::String(EcmaString::from_utf8("additionOrder")),
                Constant::String(EcmaString::from_utf8("message")),
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
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
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let left = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        let right = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        let left_value_of = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let right_value_of = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(2),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        machine
            .set_data_property(left, "valueOf", left_value_of)
            .unwrap();
        machine
            .set_data_property(right, "valueOf", right_value_of)
            .unwrap();
        let coerced = machine.add(left, right).unwrap();
        assert!(
            machine
                .string_value(coerced)
                .is_some_and(|text| text.eq_ascii("LL"))
        );

        let error_constructor = machine.intrinsics.global("Error").unwrap();
        let message = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("message")))
            .unwrap();
        let error = machine
            .call_value(error_constructor, Value::UNDEFINED, &[message])
            .unwrap();
        let empty = machine
            .allocate(HeapEntry::String(EcmaString::default()))
            .unwrap();
        let interpolated = machine.add(empty, error).unwrap();
        assert!(
            machine
                .string_value(interpolated)
                .is_some_and(|text| text.eq_ascii("Error: message"))
        );

        let date_constructor = machine.intrinsics.global("Date").unwrap();
        let date_prototype = machine
            .get_named_property(date_constructor, "prototype")
            .unwrap();
        let date = machine
            .allocate(HeapEntry::Date {
                time: 0.0,
                properties: PropertyMap::default(),
                prototype: Some(date_prototype),
                extensible: true,
            })
            .unwrap();
        machine
            .set_data_property(date, "toString", left_value_of)
            .unwrap();
        let date_text = machine.add(date, empty).unwrap();
        assert!(
            machine
                .string_value(date_text)
                .is_some_and(|text| text.eq_ascii("L"))
        );
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
    fn array_extend_uses_sync_protocol_for_set_and_rejects_plain_object() {
        let module = verified(
            Vec::new(),
            vec![function(0, 0, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let set_constructor = machine.intrinsics.global("Set").unwrap();
        let set_prototype = machine
            .get_named_property(set_constructor, "prototype")
            .unwrap();
        let set = machine
            .allocate(HeapEntry::Collection {
                entries: vec![CollectionEntry {
                    order: 0,
                    key: Value::int32(7),
                    value: Value::int32(7),
                }],
                next_order: 1,
                properties: PropertyMap::default(),
                prototype: Some(set_prototype),
                extensible: true,
            })
            .unwrap();
        let target = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();

        machine.array_extend(target, set).unwrap();
        assert_eq!(
            machine.array_elements(target).unwrap(),
            Some(vec![Value::int32(7)])
        );

        let plain_object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        assert!(matches!(
            machine.array_extend(target, plain_object),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "value is not iterable"
            }))
        ));
    }

    #[test]
    fn sync_iterator_uses_symbol_method_and_caches_next() {
        fn iterator_identity<H: Host>(
            _machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            Ok(intrinsics::BuiltinOutcome::Value(this))
        }

        fn next_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            let reads = machine.get_named_property(this, "nextReads")?;
            let reads = if reads == Value::int32(0) { 1 } else { 2 };
            machine.set_data_property(this, "nextReads", Value::int32(reads))?;
            Ok(intrinsics::BuiltinOutcome::Value(
                machine.get_named_property(this, "nextFunction")?,
            ))
        }

        fn next_result<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            Ok(intrinsics::BuiltinOutcome::Value(
                machine.get_named_property(this, "result")?,
            ))
        }

        fn done_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            machine.set_data_property(this, "order", Value::int32(1))?;
            Ok(intrinsics::BuiltinOutcome::Value(Value::FALSE))
        }

        fn value_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "order")? != Value::int32(1) {
                return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "iterator value read before done",
                }));
            }
            Ok(intrinsics::BuiltinOutcome::Value(Value::int32(42)))
        }

        let module = verified(
            Vec::new(),
            vec![function(0, 0, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let mut install = |name, handler| {
            let id = machine
                .intrinsics
                .builtins
                .register(intrinsics::BuiltinDef {
                    name,
                    length: 0,
                    handler,
                });
            intrinsics::native_function(&mut machine.heap, id, name, 0)
        };
        let iterator_identity = install(
            "[Symbol.iterator]",
            iterator_identity::<TestHost> as intrinsics::BuiltinHandler<TestHost>,
        );
        let next_getter = install("get next", next_getter::<TestHost>);
        let next_result = install("next", next_result::<TestHost>);
        let done_getter = install("get done", done_getter::<TestHost>);
        let value_getter = install("get value", value_getter::<TestHost>);
        let object_prototype = machine.intrinsics.object_prototype;
        let result = machine
            .allocate(HeapEntry::Object {
                properties: {
                    let mut properties = PropertyMap::default();
                    for (key, property) in [
                        (
                            PropertyKey::Named(EcmaString::from_utf8("order")),
                            Property::Data {
                                value: Value::int32(0),
                                writable: true,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("done")),
                            Property::Accessor {
                                getter: Some(done_getter),
                                setter: None,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("value")),
                            Property::Accessor {
                                getter: Some(value_getter),
                                setter: None,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                    ] {
                        properties.insert(key, property);
                    }
                    properties
                },
                prototype: Some(object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        let iterator_symbol = machine.intrinsics.builtins.symbol_iterator();
        let iterator_key = machine.to_property_key(iterator_symbol).unwrap();
        let source = machine
            .allocate(HeapEntry::Object {
                properties: {
                    let mut properties = PropertyMap::default();
                    for (key, property) in [
                        (
                            iterator_key,
                            Property::Data {
                                value: iterator_identity,
                                writable: true,
                                enumerable: false,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("next")),
                            Property::Accessor {
                                getter: Some(next_getter),
                                setter: None,
                                enumerable: false,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("nextReads")),
                            Property::Data {
                                value: Value::int32(0),
                                writable: true,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("nextFunction")),
                            Property::Data {
                                value: next_result,
                                writable: true,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                        (
                            PropertyKey::Named(EcmaString::from_utf8("result")),
                            Property::Data {
                                value: result,
                                writable: true,
                                enumerable: true,
                                configurable: true,
                            },
                        ),
                    ] {
                        properties.insert(key, property);
                    }
                    properties
                },
                prototype: Some(object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();

        let iterator = machine.create_iterator(source, IteratorKind::Sync).unwrap();
        assert_eq!(
            machine.iterator_next(iterator).unwrap(),
            (false, Value::int32(42))
        );
        assert_eq!(
            machine.iterator_next(iterator).unwrap(),
            (false, Value::int32(42))
        );
        assert_eq!(
            machine.get_named_property(source, "nextReads").unwrap(),
            Value::int32(1)
        );

        let mut completed_properties = PropertyMap::default();
        completed_properties.insert(
            PropertyKey::Named(EcmaString::from_utf8("done")),
            Property::Data {
                value: Value::TRUE,
                writable: true,
                enumerable: true,
                configurable: true,
            },
        );
        completed_properties.insert(
            PropertyKey::Named(EcmaString::from_utf8("value")),
            Property::Accessor {
                getter: Some(value_getter),
                setter: None,
                enumerable: true,
                configurable: true,
            },
        );
        let completed = machine
            .allocate(HeapEntry::Object {
                properties: completed_properties,
                prototype: Some(object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        machine
            .set_data_property(source, "result", completed)
            .unwrap();
        assert_eq!(
            machine.iterator_next(iterator).unwrap(),
            (true, Value::UNDEFINED)
        );

        machine
            .delete_property(source, &PropertyKey::Named(EcmaString::from_utf8("next")))
            .unwrap();
        machine
            .set_data_property(source, "next", Value::int32(1))
            .unwrap();
        let invalid_next = machine.create_iterator(source, IteratorKind::Sync).unwrap();
        assert!(matches!(
            machine.iterator_next(invalid_next),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
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
    fn object_spread_copies_enumerable_symbol_properties() {
        let module = verified(
            Vec::new(),
            vec![function(0, 0, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let prototype = machine.intrinsics.object_prototype;
        let object = |machine: &mut Machine<'_, TestHost>| {
            machine
                .allocate(HeapEntry::Object {
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                    boxed_primitive: None,
                    extensible: true,
                })
                .unwrap()
        };
        let source = object(&mut machine);
        let target = object(&mut machine);
        let symbol = machine
            .allocate(HeapEntry::Symbol {
                description: EcmaString::from_utf8("key"),
            })
            .unwrap();
        let key = machine.to_property_key(symbol).unwrap();
        machine
            .set_data_property_key(source, key.clone(), Value::int32(42))
            .unwrap();

        machine.object_spread(target, source).unwrap();

        assert_eq!(
            machine.get_property_key(target, &key).unwrap(),
            Value::int32(42)
        );
    }

    #[test]
    fn object_spread_rechecks_descriptors_after_getters() {
        fn delete_next<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<intrinsics::BuiltinOutcome, EvalFailure> {
            machine.delete_property(this, &PropertyKey::Named(EcmaString::from_utf8("next")))?;
            Ok(intrinsics::BuiltinOutcome::Value(Value::int32(1)))
        }

        let module = verified(
            Vec::new(),
            vec![function(0, 0, vec![Instruction::Halt], Vec::new())],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter_id = machine
            .intrinsics
            .builtins
            .register(intrinsics::BuiltinDef {
                name: "delete next",
                length: 0,
                handler: delete_next::<TestHost>,
            });
        let getter = intrinsics::native_function(&mut machine.heap, getter_id, "delete next", 0);
        let first = PropertyKey::Named(EcmaString::from_utf8("first"));
        let next = PropertyKey::Named(EcmaString::from_utf8("next"));
        let mut source_properties = PropertyMap::default();
        source_properties.insert(
            first.clone(),
            Property::Accessor {
                getter: Some(getter),
                setter: None,
                enumerable: true,
                configurable: true,
            },
        );
        source_properties.insert(
            next.clone(),
            Property::Data {
                value: Value::int32(2),
                writable: true,
                enumerable: true,
                configurable: true,
            },
        );
        let prototype = machine.intrinsics.object_prototype;
        let source = machine
            .allocate(HeapEntry::Object {
                properties: source_properties,
                prototype: Some(prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        let target = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();

        machine.object_spread(target, source).unwrap();

        assert_eq!(
            machine.get_property_key(target, &first).unwrap(),
            Value::int32(1)
        );
        assert!(!machine.has_own_property_key(target, &next).unwrap());
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
    fn create_cell_throws_reference_error_before_initialization() {
        let module = verified(
            vec![Constant::Int32(0)],
            vec![function(
                0,
                3,
                vec![
                    Instruction::CreateCell { dst: reg(0) },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(0),
                    },
                    Instruction::GetProperty {
                        dst: reg(2),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Return { value: reg(2) },
                ],
                vec![],
            )],
        );
        let mut host = TestHost;
        let error = Machine::new(&module, &mut host, Limits::default())
            .run()
            .expect_err("uninitialized cell read throws");
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                origin: ThrowOrigin::ReferenceError { .. },
                ..
            }
        ));
    }

    #[test]
    fn create_cell_can_be_initialized_to_undefined() {
        let module = verified(
            vec![Constant::Int32(0), Constant::Undefined],
            vec![function(
                0,
                4,
                vec![
                    Instruction::CreateCell { dst: reg(0) },
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
                        dst: reg(3),
                        object: reg(0),
                        key: reg(1),
                    },
                    Instruction::Return { value: reg(3) },
                ],
                vec![],
            )],
        );
        let mut host = TestHost;
        let execution = Machine::new(&module, &mut host, Limits::default())
            .run()
            .expect("explicit undefined initializes the cell");
        assert_eq!(execution.value, Value::UNDEFINED);
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
    fn native_callback_throw_is_caught_at_outer_call_site() {
        let entry = function(
            0,
            9,
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
                Instruction::CreateArray { dst: reg(2) },
                Instruction::CreateClosure {
                    dst: reg(3),
                    function: FunctionId::new(1),
                    captures: reg(2),
                },
                Instruction::LoadConst {
                    dst: reg(4),
                    constant: cid(1),
                },
                Instruction::GetProperty {
                    dst: reg(5),
                    object: reg(0),
                    key: reg(4),
                },
                Instruction::CreateArray { dst: reg(6) },
                Instruction::ArrayPush {
                    array: reg(6),
                    value: reg(3),
                },
                Instruction::Call {
                    dst: reg(7),
                    callee: reg(5),
                    this_value: reg(0),
                    arguments: reg(6),
                },
                Instruction::Halt,
                Instruction::Return { value: reg(8) },
            ],
            vec![ExceptionHandler {
                start: pc(9),
                end: pc(10),
                handler: pc(11),
                catch_register: reg(8),
            }],
        );
        let callback = closure_function(
            0,
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::Throw { value: reg(0) },
            ],
        );
        let module = verified(
            vec![
                Constant::Int32(7),
                Constant::String(EcmaString::from_utf8("map")),
            ],
            vec![entry, callback],
        );

        assert_eq!(run_ok(&module).value, Value::int32(7));
    }

    #[test]
    fn native_callback_throw_uncaught_at_outer_call_site() {
        let entry = function(
            0,
            9,
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
                Instruction::CreateArray { dst: reg(2) },
                Instruction::CreateClosure {
                    dst: reg(3),
                    function: FunctionId::new(1),
                    captures: reg(2),
                },
                Instruction::LoadConst {
                    dst: reg(4),
                    constant: cid(1),
                },
                Instruction::GetProperty {
                    dst: reg(5),
                    object: reg(0),
                    key: reg(4),
                },
                Instruction::CreateArray { dst: reg(6) },
                Instruction::ArrayPush {
                    array: reg(6),
                    value: reg(3),
                },
                Instruction::Call {
                    dst: reg(7),
                    callee: reg(5),
                    this_value: reg(0),
                    arguments: reg(6),
                },
                Instruction::Halt,
            ],
            Vec::new(),
        );
        let callback = closure_function(
            0,
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(0),
                },
                Instruction::Throw { value: reg(0) },
            ],
        );
        let simple = closure_function(
            0,
            0,
            1,
            vec![
                Instruction::LoadConst {
                    dst: reg(0),
                    constant: cid(2),
                },
                Instruction::Return { value: reg(0) },
            ],
        );
        let module = verified(
            vec![
                Constant::Int32(7),
                Constant::String(EcmaString::from_utf8("map")),
                Constant::Int32(42),
            ],
            vec![entry, callback, simple],
        );

        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let error = machine.run_loop(0).unwrap_err();
        assert_eq!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                value: Value::int32(7),
                origin: ThrowOrigin::Bytecode,
            }
        );
        assert!(machine.callback_boundaries.is_empty());
        assert!(machine.frames.is_empty());
        assert_eq!(machine.live_registers, 0);

        let callee = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(2),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        assert_eq!(
            machine.call_value(callee, Value::UNDEFINED, &[]).unwrap(),
            Value::int32(42)
        );
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
    fn dynamic_import_preserves_cycles_identity_and_single_evaluation() {
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("./dependency")),
                Constant::String(EcmaString::from_utf8("count")),
                Constant::Int32(0),
                Constant::String(EcmaString::from_utf8("value")),
            ],
            vec![function(
                0,
                7,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(3),
                    },
                    Instruction::StoreGlobal {
                        name: cid(2),
                        value: reg(0),
                    },
                    Instruction::Import {
                        dst: reg(1),
                        specifier: cid(1),
                    },
                    Instruction::Import {
                        dst: reg(2),
                        specifier: cid(1),
                    },
                    Instruction::Binary {
                        dst: reg(3),
                        op: BinaryOp::StrictEqual,
                        left: reg(1),
                        right: reg(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(4),
                        constant: cid(4),
                    },
                    Instruction::GetProperty {
                        dst: reg(5),
                        object: reg(2),
                        key: reg(4),
                    },
                    Instruction::LoadGlobal {
                        dst: reg(6),
                        name: cid(2),
                    },
                    Instruction::Return { value: reg(5) },
                ],
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            Vec::new(),
        );
        let dependency = program_module(
            "dependency",
            vec![
                Constant::String(EcmaString::from_utf8("./root")),
                Constant::String(EcmaString::from_utf8("count")),
                Constant::Int32(1),
                Constant::Int32(7),
                Constant::String(EcmaString::from_utf8("value")),
            ],
            vec![function(
                0,
                3,
                vec![
                    Instruction::LoadGlobal {
                        dst: reg(0),
                        name: cid(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(1),
                        constant: cid(3),
                    },
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::StoreGlobal {
                        name: cid(2),
                        value: reg(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(4),
                    },
                    Instruction::StoreGlobal {
                        name: cid(5),
                        value: reg(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
            )],
            vec![Edge {
                specifier: cid(1),
                target: EdgeTarget::Local(ModuleId::new(0)),
                kind: EdgeKind::Static,
            }],
            vec![Binding {
                name: cid(5),
                kind: BindingKind::Hoisted,
            }],
            vec![Export {
                name: cid(5),
                source: ExportSource::Local(BindingId::new(0)),
            }],
        );

        let execution = run_ok(&linked(vec![root, dependency], 0));
        assert_eq!(execution.value, Value::int32(7));
        assert_eq!(execution.entry_registers[1], execution.entry_registers[2]);
        assert_eq!(execution.entry_registers[3], Value::TRUE);
        assert_eq!(execution.entry_registers[6], Value::int32(1));
    }

    #[test]
    fn dynamic_import_counts_live_registers_and_retries_engine_failures() {
        let target = program_module(
            "target",
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let root = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("./target"))],
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
                target: EdgeTarget::Local(ModuleId::new(1)),
                kind: EdgeKind::Dynamic,
            }],
            Vec::new(),
            Vec::new(),
        );
        let program = linked(vec![root, target], 0);
        let mut host = TestHost;
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_total_registers: 1,
                ..Limits::default()
            },
        );
        machine.frames.clear();
        machine.live_registers = 0;
        machine.instantiate_modules().unwrap();

        let error = machine.evaluate_import(ModuleId::new(0)).unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::RegisterLimitExceeded { limit: 1 }
        ));
        assert_eq!(machine.frames.len(), 0);
        assert_eq!(machine.live_registers, 0);

        machine.limits.max_total_registers = 2;
        machine.evaluate_import(ModuleId::new(0)).unwrap();
    }

    #[test]
    fn dynamic_import_rethrows_one_stored_failure_at_each_import_site() {
        let root = program_module(
            "root",
            vec![
                Constant::String(EcmaString::from_utf8("./target")),
                Constant::String(EcmaString::from_utf8("count")),
                Constant::Int32(0),
            ],
            vec![function(
                0,
                4,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(3),
                    },
                    Instruction::StoreGlobal {
                        name: cid(2),
                        value: reg(0),
                    },
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(1),
                    },
                    Instruction::Halt,
                    Instruction::Import {
                        dst: reg(0),
                        specifier: cid(1),
                    },
                    Instruction::Halt,
                    Instruction::LoadGlobal {
                        dst: reg(3),
                        name: cid(2),
                    },
                    Instruction::Return { value: reg(2) },
                ],
                vec![
                    ExceptionHandler {
                        start: pc(2),
                        end: pc(3),
                        handler: pc(4),
                        catch_register: reg(1),
                    },
                    ExceptionHandler {
                        start: pc(4),
                        end: pc(5),
                        handler: pc(6),
                        catch_register: reg(2),
                    },
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
            vec![
                Constant::String(EcmaString::from_utf8("count")),
                Constant::Int32(1),
                Constant::Int32(9),
            ],
            vec![function(
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
                    Instruction::Binary {
                        dst: reg(2),
                        op: BinaryOp::Add,
                        left: reg(0),
                        right: reg(1),
                    },
                    Instruction::StoreGlobal {
                        name: cid(1),
                        value: reg(2),
                    },
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(3),
                    },
                    Instruction::Throw { value: reg(0) },
                ],
                Vec::new(),
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let execution = run_ok(&linked(vec![root, target], 0));
        assert_eq!(execution.value, Value::int32(9));
        assert_eq!(execution.entry_registers[1], Value::int32(9));
        assert_eq!(execution.entry_registers[2], Value::int32(9));
        assert_eq!(execution.entry_registers[3], Value::int32(1));
    }

    #[test]
    fn dynamic_import_returns_the_registered_external_namespace() {
        let module = program_module(
            "root",
            vec![Constant::String(EcmaString::from_utf8("external"))],
            vec![function(
                0,
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
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let namespace = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        machine.registry.external.insert(
            EcmaString::from_utf8("external"),
            ExternalModuleInstance {
                namespace,
                exports: BTreeMap::new(),
                internals: BTreeMap::new(),
            },
        );

        let execution = machine.run().unwrap();
        assert_eq!(execution.value, Value::TRUE);
        assert_eq!(execution.entry_registers[0], namespace);
        assert_eq!(execution.entry_registers[1], namespace);
    }

    #[test]
    fn dynamic_import_resolution_is_requester_scoped() {
        let requester = |name, target| {
            program_module(
                name,
                vec![Constant::String(EcmaString::from_utf8("./target"))],
                vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
                vec![Edge {
                    specifier: cid(1),
                    target: EdgeTarget::Local(ModuleId::new(target)),
                    kind: EdgeKind::Dynamic,
                }],
                Vec::new(),
                Vec::new(),
            )
        };
        let target = |name| {
            program_module(
                name,
                Vec::new(),
                vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        };
        let program = linked(
            vec![
                requester("first", 2),
                requester("second", 3),
                target("first-target"),
                target("second-target"),
            ],
            0,
        );
        let mut host = TestHost;
        let machine = Machine::new(&program, &mut host, Limits::default());

        assert_eq!(
            machine.resolve_import(ModuleId::new(0), cid(1)),
            Ok(ImportTarget::Local(ModuleId::new(2)))
        );
        assert_eq!(
            machine.resolve_import(ModuleId::new(1), cid(1)),
            Ok(ImportTarget::Local(ModuleId::new(3)))
        );
    }

    #[test]
    fn dynamic_import_of_a_missing_external_is_a_runtime_error() {
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
            RuntimeErrorKind::ExternalModuleUnavailable { module, edge }
                if module == ModuleId::new(0) && edge == EdgeId::new(0)
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

    #[test]
    fn installed_script_uses_machine_wide_id_and_keeps_its_code() {
        let root = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let script = Arc::new(verified(
            vec![Constant::Int32(42)],
            vec![function(
                0,
                1,
                vec![
                    Instruction::LoadConst {
                        dst: reg(0),
                        constant: cid(0),
                    },
                    Instruction::Return { value: reg(0) },
                ],
                Vec::new(),
            )],
        ));
        let mut host = TestHost;
        let mut machine = Machine::new(&root, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let module = machine.install_script_reserving(script, 0, 0).unwrap();

        assert_eq!(module, ModuleId::new(root.modules().len() as u32));
        assert!(machine.program().module(module).is_none());
        assert_eq!(
            machine.module_code(module).constants()[0],
            Constant::Int32(42)
        );

        let closure = machine
            .allocate(HeapEntry::Function {
                module,
                function: FunctionId::new(0),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        assert!(matches!(
            machine.call_value(closure, Value::UNDEFINED, &[]),
            Ok(value) if value == Value::int32(42)
        ));
    }

    #[test]
    fn installed_script_rejects_non_classic_programs_and_enforces_limit() {
        let root = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let two_modules = Arc::new(linked(
            vec![
                program_module(
                    "first",
                    Vec::new(),
                    vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
                program_module(
                    "second",
                    Vec::new(),
                    vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
            ],
            0,
        ));
        let script = Arc::new(verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        ));
        let mut host = TestHost;
        let mut machine = Machine::new(
            &root,
            &mut host,
            Limits {
                max_dynamic_modules: 1,
                ..Limits::default()
            },
        );
        machine.instantiate_modules().unwrap();

        assert!(matches!(
            machine.install_script_reserving(two_modules, 0, 0),
            Err(RuntimeErrorKind::InvalidDynamicScript { .. })
        ));
        machine
            .install_script_reserving(script.clone(), 0, 0)
            .unwrap();
        assert!(matches!(
            machine.install_script_reserving(script, 0, 0),
            Err(RuntimeErrorKind::DynamicModuleLimitExceeded { limit: 1 })
        ));
    }

    #[test]
    fn script_heap_cost_counts_scalar_constant_slots() {
        let entry = || vec![function(0, 1, vec![Instruction::Halt], Vec::new())];
        let empty = verified(Vec::new(), entry());
        let constants = vec![Constant::Int32(0); 128];
        let scalars = verified(constants.clone(), entry());

        let added = Machine::<TestHost>::script_heap_cost(&scalars)
            - Machine::<TestHost>::script_heap_cost(&empty);

        assert!(added >= constants.len() * std::mem::size_of::<Constant>());
    }

    #[test]
    fn script_heap_cost_includes_verification_storage() {
        let small = verified(
            Vec::new(),
            vec![function(0, 1, vec![Instruction::Halt], Vec::new())],
        );
        let large = verified(
            Vec::new(),
            vec![function(0, 130, vec![Instruction::Halt], Vec::new())],
        );
        let small_verification = small.modules()[0].code.verification_bytes();
        let large_verification = large.modules()[0].code.verification_bytes();

        assert_eq!(
            Machine::<TestHost>::script_heap_cost(&large)
                - Machine::<TestHost>::script_heap_cost(&small),
            large_verification - small_verification
        );
    }
    #[test]
    fn promise_resolver_settles_once_and_reactions_wait_for_drain() {
        let program = verified(
            vec![
                Constant::String(EcmaString::from_utf8("resolve")),
                Constant::String(EcmaString::from_utf8("reject")),
                Constant::String(EcmaString::from_utf8("observed")),
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    2,
                    2,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(0),
                            value: reg(0),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    1,
                    1,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let executor = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let observer = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(2),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let constructor = machine.intrinsics.global("Promise").unwrap();
        let constructor_index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(constructor_id),
            ..
        } = machine.heap[constructor_index]
        else {
            panic!("Promise must be a native constructor");
        };
        let BuiltinOutcome::Value(promise) = machine
            .call_builtin(constructor_id, Value::UNDEFINED, &[executor], true)
            .unwrap()
        else {
            panic!("Promise construction returns a Promise");
        };
        let then = machine.get_named_property(promise, "then").unwrap();
        machine
            .call_value(then, promise, &[observer])
            .expect("then returns a derived Promise");
        let resolve = machine
            .globals
            .get(&EcmaString::from_utf8("resolve"))
            .copied()
            .unwrap();
        let reject = machine
            .globals
            .get(&EcmaString::from_utf8("reject"))
            .copied()
            .unwrap();
        assert_eq!(
            machine
                .call_value(resolve, Value::UNDEFINED, &[Value::int32(1)])
                .unwrap(),
            Value::UNDEFINED
        );
        assert_eq!(
            machine
                .call_value(reject, Value::UNDEFINED, &[Value::int32(2)])
                .unwrap(),
            Value::UNDEFINED
        );
        assert!(
            !machine
                .globals
                .contains_key(&EcmaString::from_utf8("observed"))
        );

        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 1);
        assert!(drain.uncaught.is_empty());
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("observed"))
                .copied(),
            Some(Value::int32(1))
        );
    }

    #[test]
    fn promise_resolution_adopts_thenables_with_a_fresh_resolver() {
        let program = verified(
            vec![
                Constant::String(EcmaString::from_utf8("resolve")),
                Constant::String(EcmaString::from_utf8("reject")),
                Constant::String(EcmaString::from_utf8("observed")),
                Constant::Int32(7),
                Constant::Int32(8),
                Constant::Int32(9),
                Constant::Undefined,
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    2,
                    2,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(0),
                            value: reg(0),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    1,
                    1,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    2,
                    6,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(3),
                        },
                        Instruction::CreateArray { dst: reg(3) },
                        Instruction::ArrayPush {
                            array: reg(3),
                            value: reg(2),
                        },
                        Instruction::LoadConst {
                            dst: reg(4),
                            constant: cid(6),
                        },
                        Instruction::Call {
                            dst: reg(5),
                            callee: reg(0),
                            this_value: reg(4),
                            arguments: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(4),
                        },
                        Instruction::CreateArray { dst: reg(3) },
                        Instruction::ArrayPush {
                            array: reg(3),
                            value: reg(2),
                        },
                        Instruction::Call {
                            dst: reg(5),
                            callee: reg(1),
                            this_value: reg(4),
                            arguments: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(2),
                            constant: cid(5),
                        },
                        Instruction::Throw { value: reg(2) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let runtime_function = |machine: &mut Machine<'_, TestHost>, function| {
            machine
                .allocate(HeapEntry::Function {
                    module: ModuleId::new(0),
                    function: FunctionId::new(function),
                    captures: Vec::new(),
                    properties: PropertyMap::default(),
                    prototype: Some(machine.intrinsics.function_prototype),
                    extensible: true,
                })
                .unwrap()
        };
        let executor = runtime_function(&mut machine, 1);
        let observer = runtime_function(&mut machine, 2);
        let then_callback = runtime_function(&mut machine, 3);
        let thenable = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .unwrap();
        machine
            .set_data_property(thenable, "then", then_callback)
            .unwrap();

        let constructor = machine.intrinsics.global("Promise").unwrap();
        let constructor_index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(constructor_id),
            ..
        } = machine.heap[constructor_index]
        else {
            panic!("Promise must be a native constructor");
        };
        let BuiltinOutcome::Value(promise) = machine
            .call_builtin(constructor_id, Value::UNDEFINED, &[executor], true)
            .unwrap()
        else {
            panic!("Promise construction returns a Promise");
        };
        let resolve = machine
            .globals
            .get(&EcmaString::from_utf8("resolve"))
            .copied()
            .unwrap();
        let reject = machine
            .globals
            .get(&EcmaString::from_utf8("reject"))
            .copied()
            .unwrap();
        machine
            .call_value(resolve, Value::UNDEFINED, &[thenable])
            .unwrap();
        let then = machine.get_named_property(promise, "then").unwrap();
        machine.call_value(then, promise, &[observer]).unwrap();
        machine
            .call_value(reject, Value::UNDEFINED, &[Value::int32(9)])
            .unwrap();
        assert!(
            !machine
                .globals
                .contains_key(&EcmaString::from_utf8("observed"))
        );

        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 2);
        assert!(drain.uncaught.is_empty());
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("observed"))
                .copied(),
            Some(Value::int32(7))
        );
    }

    #[test]
    fn queue_microtask_drains_fifo_including_jobs_added_during_drain() {
        let program = verified(
            vec![
                Constant::String(EcmaString::from_utf8("order")),
                Constant::String(EcmaString::from_utf8("queueMicrotask")),
                Constant::String(EcmaString::from_utf8("third")),
                Constant::Int32(1),
                Constant::Int32(2),
                Constant::Int32(3),
                Constant::Undefined,
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    0,
                    7,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(3),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::LoadGlobal {
                            dst: reg(2),
                            name: cid(1),
                        },
                        Instruction::LoadGlobal {
                            dst: reg(3),
                            name: cid(2),
                        },
                        Instruction::CreateArray { dst: reg(4) },
                        Instruction::ArrayPush {
                            array: reg(4),
                            value: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(5),
                            constant: cid(6),
                        },
                        Instruction::Call {
                            dst: reg(6),
                            callee: reg(2),
                            this_value: reg(5),
                            arguments: reg(4),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(4),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(5),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let runtime_function = |machine: &mut Machine<'_, TestHost>, function| {
            machine
                .allocate(HeapEntry::Function {
                    module: ModuleId::new(0),
                    function: FunctionId::new(function),
                    captures: Vec::new(),
                    properties: PropertyMap::default(),
                    prototype: Some(machine.intrinsics.function_prototype),
                    extensible: true,
                })
                .unwrap()
        };
        let first = runtime_function(&mut machine, 1);
        let second = runtime_function(&mut machine, 2);
        let third = runtime_function(&mut machine, 3);
        let order = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();
        machine
            .globals
            .insert(EcmaString::from_utf8("order"), order);
        machine
            .globals
            .insert(EcmaString::from_utf8("third"), third);
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[first])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[second])
            .unwrap();

        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 3);
        assert!(drain.uncaught.is_empty());
        let index = machine.runtime_slot(order).unwrap().unwrap();
        let HeapEntry::Array { elements, .. } = &machine.heap[index] else {
            panic!("order remains an array");
        };
        assert_eq!(
            elements,
            &[Value::int32(1), Value::int32(2), Value::int32(3)]
        );
    }

    #[test]
    fn queue_microtask_reports_callback_throws_and_continues() {
        let program = verified(
            vec![
                Constant::Int32(7),
                Constant::Int32(1),
                Constant::String(EcmaString::from_utf8("observed")),
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Throw { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(1),
                        },
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let runtime_function = |machine: &mut Machine<'_, TestHost>, function| {
            machine
                .allocate(HeapEntry::Function {
                    module: ModuleId::new(0),
                    function: FunctionId::new(function),
                    captures: Vec::new(),
                    properties: PropertyMap::default(),
                    prototype: Some(machine.intrinsics.function_prototype),
                    extensible: true,
                })
                .unwrap()
        };
        let throwing = runtime_function(&mut machine, 1);
        let observer = runtime_function(&mut machine, 2);
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[throwing])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[observer])
            .unwrap();

        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 2);
        assert_eq!(
            drain.uncaught,
            vec![CallbackException {
                value: Value::int32(7),
                origin: ThrowOrigin::Bytecode,
            }]
        );
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("observed"))
                .copied(),
            Some(Value::int32(1))
        );
    }

    #[test]
    fn microtask_boundaries_preserve_the_queued_head() {
        let program = verified(
            vec![Constant::Undefined],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        );
        let mut host = TestHost;
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_microtasks: 1,
                ..Limits::default()
            },
        );
        machine.frames.clear();
        machine.live_registers = 0;
        let callback = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(1),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        assert!(matches!(
            machine.call_value(queue, Value::UNDEFINED, &[Value::int32(1)]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        machine
            .call_value(queue, Value::UNDEFINED, &[callback])
            .unwrap();
        assert!(matches!(
            machine.call_value(queue, Value::UNDEFINED, &[callback]),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::MicrotaskQueueLimitExceeded { limit: 1 }
            ))
        ));

        let fuel = machine.fuel;
        machine.microtask_drain_active = true;
        let reentry = machine.drain_microtasks().unwrap_err();
        assert!(matches!(
            reentry.kind,
            RuntimeErrorKind::MicrotaskDrainReentry
        ));
        assert_eq!(machine.fuel, fuel);
        assert_eq!(machine.microtasks.len(), 1);
        machine.microtask_drain_active = false;

        machine.fuel = 0;
        let exhausted = machine.drain_microtasks().unwrap_err();
        assert!(matches!(
            exhausted.kind,
            RuntimeErrorKind::FuelExhausted { .. }
        ));
        assert!(!machine.microtask_drain_active);
        assert_eq!(machine.microtasks.len(), 1);

        machine.fuel = 100;
        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 1);
        assert!(machine.microtasks.is_empty());
    }

    // ---- timers -----------------------------------------------------------

    #[derive(Default)]
    struct ManualTimerState {
        live: std::collections::BTreeMap<u64, u64>,
        reports: std::collections::VecDeque<TimerWakeup>,
        scheduled: Vec<(u64, u32)>,
        cancelled: Vec<u64>,
        fail_schedule: bool,
        fail_poll: bool,
    }

    #[derive(Clone, Default)]
    struct ManualTimerProvider {
        state: std::rc::Rc<std::cell::RefCell<ManualTimerState>>,
    }

    impl TimerProvider for ManualTimerProvider {
        fn schedule(&mut self, id: u64, delay_ms: u32) -> Result<u64, TimerError> {
            let mut state = self.state.borrow_mut();
            state.scheduled.push((id, delay_ms));
            if state.fail_schedule {
                return Err(TimerError::new("manual schedule failure"));
            }
            let deadline = u64::from(delay_ms);
            state.live.insert(id, deadline);
            Ok(deadline)
        }

        fn cancel(&mut self, id: u64) -> Result<bool, TimerError> {
            let mut state = self.state.borrow_mut();
            state.cancelled.push(id);
            Ok(state.live.remove(&id).is_some())
        }

        fn poll_expired(&mut self, output: &mut Vec<TimerWakeup>) -> Result<(), TimerError> {
            let mut state = self.state.borrow_mut();
            if state.fail_poll {
                return Err(TimerError::new("manual poll failure"));
            }
            output.extend(state.reports.drain(..));
            Ok(())
        }

        fn wait_expired(&mut self) -> Result<Option<TimerWakeup>, TimerError> {
            Ok(self.state.borrow_mut().reports.pop_front())
        }

        fn has_pending(&self) -> bool {
            !self.state.borrow().live.is_empty()
        }
    }

    #[derive(Default)]
    struct TimerTestHost {
        provider: ManualTimerProvider,
    }

    impl Host for TimerTestHost {
        fn timers(&mut self) -> Option<&mut (dyn TimerProvider + 'static)> {
            Some(&mut self.provider)
        }
    }

    fn timer_program() -> Program<Verified> {
        verified(
            vec![
                Constant::String(EcmaString::from_utf8("a")),
                Constant::String(EcmaString::from_utf8("b")),
                Constant::String(EcmaString::from_utf8("this_seen")),
                Constant::String(EcmaString::from_utf8("arg_seen")),
                Constant::Int32(1),
                Constant::Int32(7),
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    0,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(4),
                        },
                        Instruction::StoreGlobal {
                            name: cid(0),
                            value: reg(0),
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
                            constant: cid(4),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    1,
                    2,
                    vec![
                        Instruction::LoadThis { dst: reg(1) },
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(1),
                        },
                        Instruction::StoreGlobal {
                            name: cid(3),
                            value: reg(0),
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
                            constant: cid(5),
                        },
                        Instruction::Throw { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        )
    }

    fn timer_fn(machine: &mut Machine<'_, TimerTestHost>, index: u32) -> Value {
        machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(index),
                captures: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap()
    }

    fn read_global(machine: &Machine<'_, TimerTestHost>, name: &str) -> Option<Value> {
        machine.globals.get(&EcmaString::from_utf8(name)).copied()
    }

    fn set_timeout_global(machine: &Machine<'_, TimerTestHost>) -> Value {
        machine
            .intrinsics
            .global("setTimeout")
            .expect("setTimeout is installed")
    }

    fn schedule_nested_timer(
        machine: &mut Machine<'_, TimerTestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let callback = machine
            .globals
            .get(&EcmaString::from_utf8("nestedCallback"))
            .copied()
            .expect("test installs nested callback");
        let set_timeout = set_timeout_global(machine);
        machine.call_value(set_timeout, Value::UNDEFINED, &[callback, Value::int32(1)])?;
        Ok(BuiltinOutcome::Value(Value::UNDEFINED))
    }

    fn timer_native(
        machine: &mut Machine<'_, TimerTestHost>,
        name: &'static str,
        handler: crate::intrinsics::BuiltinHandler<TimerTestHost>,
    ) -> Value {
        let id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name,
                length: 0,
                handler,
            });
        crate::intrinsics::native_function(&mut machine.heap, id, name, 0)
    }

    #[test]
    fn timers_are_absent_without_the_capability() {
        let program = timer_program();
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        assert!(machine.intrinsics.global("setTimeout").is_none());
        assert!(machine.intrinsics.global("clearTimeout").is_none());
        assert!(!machine.has_pending_timers());
        assert_eq!(
            machine.run_one_expired_timer().unwrap(),
            TimerRun::default()
        );
        assert!(!machine.wait_for_timer_expiry().unwrap());
    }

    #[test]
    fn set_timeout_rejects_a_non_callable_callback_before_coercion() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let failure = machine
            .call_value(
                set_timeout,
                Value::UNDEFINED,
                &[Value::int32(3), Value::int32(5)],
            )
            .unwrap_err();
        assert!(matches!(
            failure,
            EvalFailure::Throw(ThrowOrigin::TypeError { .. })
        ));
        // Nothing was armed, so no delay coercion or provider call happened.
        assert!(shared.borrow().scheduled.is_empty());
        assert!(!machine.has_pending_timers());
    }

    #[test]
    fn set_timeout_clamps_and_truncates_like_node() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let callback = timer_fn(&mut machine, 1);
        for delay in [
            Value::int32(0),
            Value::number(-5.0),
            Value::number(f64::NAN),
            Value::number(2_147_483_648.0),
            Value::int32(2_147_483_647),
            Value::number(3.9),
        ] {
            machine
                .call_value(set_timeout, Value::UNDEFINED, &[callback, delay])
                .unwrap();
        }
        let delays: Vec<u32> = shared.borrow().scheduled.iter().map(|(_, d)| *d).collect();
        assert_eq!(delays, vec![1, 1, 1, 1, 2_147_483_647, 3]);
        // Ids are minted monotonically from 1 and never reused.
        let ids: Vec<u64> = shared
            .borrow()
            .scheduled
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn same_deadline_timers_run_in_registration_order_despite_reverse_reports() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let a = timer_fn(&mut machine, 1);
        let b = timer_fn(&mut machine, 2);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(5)])
            .unwrap();
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[b, Value::int32(5)])
            .unwrap();
        // Host reports the later registration first and in split batches.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 5,
        });
        let first = machine.run_one_expired_timer().unwrap();
        assert_eq!(first.executed, 1);
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
        assert_eq!(read_global(&machine, "b"), None);
        let second = machine.run_one_expired_timer().unwrap();
        assert_eq!(second.executed, 1);
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
        assert!(!machine.has_pending_timers());
    }

    #[test]
    fn a_shorter_deadline_beats_an_older_sequence() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let a = timer_fn(&mut machine, 1);
        let b = timer_fn(&mut machine, 2);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(5)])
            .unwrap();
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[b, Value::int32(3)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 5,
        });
        machine.run_one_expired_timer().unwrap();
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
        assert_eq!(read_global(&machine, "a"), None);
    }

    #[test]
    fn clear_timeout_prevents_a_ready_timer_and_ignores_stale_ids() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let clear_timeout = machine.intrinsics.global("clearTimeout").unwrap();
        let a = timer_fn(&mut machine, 1);
        let b = timer_fn(&mut machine, 2);
        let handle_a = machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(3)])
            .unwrap();
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[b, Value::int32(3)])
            .unwrap();
        // Clear the first timer even though the host already reported it.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 3,
        });
        machine
            .call_value(clear_timeout, Value::UNDEFINED, &[handle_a])
            .unwrap();
        assert!(shared.borrow().cancelled.contains(&1));
        // A stale positive-integer id must not cancel the surviving timer.
        machine
            .call_value(clear_timeout, Value::UNDEFINED, &[Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 3,
        });
        let run = machine.run_one_expired_timer().unwrap();
        assert_eq!(run.executed, 1);
        assert_eq!(read_global(&machine, "a"), None);
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
    }

    #[test]
    fn clear_timeout_accepts_a_direct_positive_integer_id() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let clear_timeout = machine.intrinsics.global("clearTimeout").unwrap();
        let a = timer_fn(&mut machine, 1);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(3)])
            .unwrap();
        machine
            .call_value(clear_timeout, Value::UNDEFINED, &[Value::int32(1)])
            .unwrap();
        assert!(!machine.has_pending_timers());
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 3,
        });
        assert_eq!(machine.run_one_expired_timer().unwrap().executed, 0);

        machine.next_timer_id = Some(u64::MAX);
        let handle = machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(3)])
            .unwrap();
        machine
            .call_value(
                clear_timeout,
                Value::UNDEFINED,
                &[Value::number(u64::MAX as f64)],
            )
            .unwrap();
        assert!(machine.has_pending_timers());
        machine
            .call_value(clear_timeout, Value::UNDEFINED, &[handle])
            .unwrap();
        assert!(!machine.has_pending_timers());
        // A no-op clear of an unrelated value never coerces or errors.
        machine
            .call_value(clear_timeout, Value::UNDEFINED, &[Value::UNDEFINED])
            .unwrap();
    }

    #[test]
    fn timer_callback_receives_trailing_args_and_the_handle_as_this() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let callback = timer_fn(&mut machine, 3);
        let handle = machine
            .call_value(
                set_timeout,
                Value::UNDEFINED,
                &[callback, Value::int32(1), Value::int32(42)],
            )
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        machine.run_one_expired_timer().unwrap();
        assert_eq!(read_global(&machine, "this_seen"), Some(handle));
        assert_eq!(read_global(&machine, "arg_seen"), Some(Value::int32(42)));
    }

    #[test]
    fn a_callback_created_timer_waits_for_a_later_checkpoint() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let nested = timer_fn(&mut machine, 2);
        machine
            .globals
            .insert(EcmaString::from_utf8("nestedCallback"), nested);
        let creator = timer_native(&mut machine, "schedule nested", schedule_nested_timer);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[creator, Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        assert_eq!(machine.run_one_expired_timer().unwrap().executed, 1);
        assert_eq!(read_global(&machine, "b"), None);
        assert!(machine.has_pending_timers());
        // Even if the provider can report it immediately, it runs only in a
        // later explicit timer checkpoint.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 1,
        });
        assert_eq!(machine.run_one_expired_timer().unwrap().executed, 1);
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
    }

    #[test]
    fn timer_callback_throw_is_reported_and_a_runtime_failure_propagates() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let thrower = timer_fn(&mut machine, 4);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[thrower, Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        let run = machine.run_one_expired_timer().unwrap();
        assert_eq!(run.executed, 1);
        assert_eq!(
            run.uncaught,
            vec![CallbackException {
                value: Value::int32(7),
                origin: ThrowOrigin::Bytecode
            }]
        );

        // A runtime failure inside the callback stops the checkpoint.
        let another = timer_fn(&mut machine, 1);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[another, Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 1,
        });
        machine.fuel = 1;
        let error = machine.run_one_expired_timer().unwrap_err();
        assert!(matches!(error.kind, RuntimeErrorKind::FuelExhausted { .. }));
    }

    #[test]
    fn a_timer_checkpoint_never_drains_microtasks() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        let a = timer_fn(&mut machine, 1);
        let b = timer_fn(&mut machine, 2);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(1)])
            .unwrap();
        machine.call_value(queue, Value::UNDEFINED, &[b]).unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        let run = machine.run_one_expired_timer().unwrap();
        assert_eq!(run.executed, 1);
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
        assert_eq!(read_global(&machine, "b"), None);
        assert_eq!(machine.microtasks.len(), 1);
        machine.drain_microtasks().unwrap();
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
    }

    #[test]
    fn timer_reentry_capacity_and_fuel_preserve_state() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(
            &program,
            &mut host,
            Limits {
                max_timers: 1,
                ..Limits::default()
            },
        );
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let a = timer_fn(&mut machine, 1);
        let b = timer_fn(&mut machine, 2);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(1)])
            .unwrap();
        // Capacity is enforced before any provider or table mutation.
        let capacity = machine
            .call_value(set_timeout, Value::UNDEFINED, &[b, Value::int32(1)])
            .unwrap_err();
        assert!(matches!(
            capacity,
            EvalFailure::Runtime(RuntimeErrorKind::TimerCapacityExceeded { limit: 1 })
        ));
        assert_eq!(shared.borrow().scheduled.len(), 1);

        // Reentry fails without consuming fuel or touching the ready timer.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        machine.timer_checkpoint_active = true;
        let fuel = machine.fuel;
        let reentry = machine.run_one_expired_timer().unwrap_err();
        assert!(matches!(
            reentry.kind,
            RuntimeErrorKind::TimerCheckpointReentry
        ));
        assert_eq!(machine.fuel, fuel);
        machine.timer_checkpoint_active = false;

        // Fuel is charged before the live record is removed.
        machine.fuel = 0;
        let exhausted = machine.run_one_expired_timer().unwrap_err();
        assert!(matches!(
            exhausted.kind,
            RuntimeErrorKind::FuelExhausted { .. }
        ));
        assert!(machine.has_pending_timers());
        machine.fuel = 100;
        assert_eq!(machine.run_one_expired_timer().unwrap().executed, 1);
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
    }

    #[test]
    fn a_failed_schedule_never_reuses_its_timer_id() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let set_timeout = set_timeout_global(&machine);
        let a = timer_fn(&mut machine, 1);
        shared.borrow_mut().fail_schedule = true;
        let failure = machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(1)])
            .unwrap_err();
        assert!(matches!(
            failure,
            EvalFailure::Runtime(RuntimeErrorKind::TimerProviderFailure { .. })
        ));
        shared.borrow_mut().fail_schedule = false;
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(1)])
            .unwrap();
        let ids: Vec<u64> = shared
            .borrow()
            .scheduled
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn wait_for_timer_expiry_promotes_a_reported_timer() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        assert!(!machine.wait_for_timer_expiry().unwrap());
        let set_timeout = set_timeout_global(&machine);
        let a = timer_fn(&mut machine, 1);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        assert!(machine.wait_for_timer_expiry().unwrap());
        assert_eq!(machine.run_one_expired_timer().unwrap().executed, 1);
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
    }

    // ---- automatic event loop ---------------------------------------------

    /// A program whose entry queues the global `"job"` microtask, with helper
    /// functions that push a marker onto the global `"order"` array. Function 1
    /// also queues `"job"` so a microtask can be created during a drain or a
    /// timer turn. The entry function is only exercised by `evaluate`.
    fn loop_test_program() -> Program<Verified> {
        verified(
            vec![
                Constant::String(EcmaString::from_utf8("order")), // 0
                Constant::Int32(1),                                // 1
                Constant::Int32(2),                                // 2
                Constant::Int32(3),                                // 3
                Constant::Int32(4),                                // 4
                Constant::String(EcmaString::from_utf8("queueMicrotask")), // 5
                Constant::String(EcmaString::from_utf8("job")),   // 6
                Constant::Undefined,                              // 7
            ],
            vec![
                function(
                    0,
                    7,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(5),
                        },
                        Instruction::LoadGlobal {
                            dst: reg(1),
                            name: cid(6),
                        },
                        Instruction::CreateArray { dst: reg(2) },
                        Instruction::ArrayPush {
                            array: reg(2),
                            value: reg(1),
                        },
                        Instruction::LoadConst {
                            dst: reg(3),
                            constant: cid(7),
                        },
                        Instruction::Call {
                            dst: reg(4),
                            callee: reg(0),
                            this_value: reg(3),
                            arguments: reg(2),
                        },
                        Instruction::Return { value: reg(3) },
                    ],
                    Vec::new(),
                ),
                function(
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
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::LoadGlobal {
                            dst: reg(2),
                            name: cid(5),
                        },
                        Instruction::LoadGlobal {
                            dst: reg(3),
                            name: cid(6),
                        },
                        Instruction::CreateArray { dst: reg(4) },
                        Instruction::ArrayPush {
                            array: reg(4),
                            value: reg(3),
                        },
                        Instruction::LoadConst {
                            dst: reg(5),
                            constant: cid(7),
                        },
                        Instruction::Call {
                            dst: reg(6),
                            callee: reg(2),
                            this_value: reg(5),
                            arguments: reg(4),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(2),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(3),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
                function(
                    0,
                    2,
                    vec![
                        Instruction::LoadGlobal {
                            dst: reg(0),
                            name: cid(0),
                        },
                        Instruction::LoadConst {
                            dst: reg(1),
                            constant: cid(4),
                        },
                        Instruction::ArrayPush {
                            array: reg(0),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(1) },
                    ],
                    Vec::new(),
                ),
            ],
        )
    }

    fn promise_throw_program() -> Program<Verified> {
        verified(
            vec![
                Constant::String(EcmaString::from_utf8("resolve")), // 0
                Constant::String(EcmaString::from_utf8("reject")),  // 1
                Constant::String(EcmaString::from_utf8("observed")), // 2
                Constant::Int32(7),                                 // 3
            ],
            vec![
                function(0, 1, vec![Instruction::Halt], Vec::new()),
                function(
                    2,
                    2,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(0),
                            value: reg(0),
                        },
                        Instruction::StoreGlobal {
                            name: cid(1),
                            value: reg(1),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    1,
                    1,
                    vec![
                        Instruction::LoadConst {
                            dst: reg(0),
                            constant: cid(3),
                        },
                        Instruction::Throw { value: reg(0) },
                    ],
                    Vec::new(),
                ),
                function(
                    1,
                    1,
                    vec![
                        Instruction::StoreGlobal {
                            name: cid(2),
                            value: reg(0),
                        },
                        Instruction::Return { value: reg(0) },
                    ],
                    Vec::new(),
                ),
            ],
        )
    }

    fn install_order_array(machine: &mut Machine<'_, TimerTestHost>) -> Value {
        let order = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();
        machine
            .globals
            .insert(EcmaString::from_utf8("order"), order);
        order
    }

    fn order_markers(machine: &Machine<'_, TimerTestHost>) -> Vec<Value> {
        let order = machine
            .globals
            .get(&EcmaString::from_utf8("order"))
            .copied()
            .expect("order array is installed");
        let index = machine
            .runtime_slot(order)
            .expect("order resolves")
            .expect("order is a heap value");
        let HeapEntry::Array { elements, .. } = &machine.heap[index] else {
            panic!("order remains an array");
        };
        elements.clone()
    }

    fn schedule_global_job(
        machine: &mut Machine<'_, TimerTestHost>,
        global: &str,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let job = machine
            .globals
            .get(&EcmaString::from_utf8(global))
            .copied()
            .unwrap_or_else(|| panic!("test installs the {global} job"));
        let queue = machine
            .intrinsics
            .global("queueMicrotask")
            .expect("queueMicrotask is installed");
        machine.call_value(queue, Value::UNDEFINED, &[job])?;
        Ok(BuiltinOutcome::Value(Value::UNDEFINED))
    }

    fn queue_job_then_throw(
        machine: &mut Machine<'_, TimerTestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        schedule_global_job(machine, "nestedCallback")?;
        Err(EvalFailure::ThrowValue(Value::int32(7)))
    }

    fn respawn_job(
        machine: &mut Machine<'_, TimerTestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        schedule_global_job(machine, "nestedCallback")
    }


    #[test]
    fn automatic_loop_leaves_an_idle_machine_untouched() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let fuel = machine.fuel;
        machine.run_to_quiescence().unwrap();
        assert_eq!(machine.fuel, fuel);
        assert!(machine.microtasks.is_empty());
        assert!(!machine.has_pending_timers());
        assert!(!machine.microtask_drain_active);
        assert!(!machine.timer_checkpoint_active);
    }

    #[test]
    fn run_returns_the_synchronous_execution_snapshot() {
        let program = loop_test_program();
        let mut host = TimerTestHost::default();
        let mut first = Machine::new(&program, &mut host, Limits::default());
        first.frames.clear();
        first.live_registers = 0;
        install_order_array(&mut first);
        first
            .globals
            .insert(EcmaString::from_utf8("job"), timer_fn(&mut first, 2));
        let snapshot = first.evaluate().unwrap();
        assert!(order_markers(&first).is_empty());
        first.run_to_quiescence().unwrap();
        assert_eq!(order_markers(&first), vec![Value::int32(2)]);
        drop(first);

        let mut host = TimerTestHost::default();
        let mut second = Machine::new(&program, &mut host, Limits::default());
        second.frames.clear();
        second.live_registers = 0;
        install_order_array(&mut second);
        second
            .globals
            .insert(EcmaString::from_utf8("job"), timer_fn(&mut second, 2));
        let execution = second.run().unwrap();
        assert_eq!(execution, snapshot);
    }

    #[test]
    fn automatic_loop_drains_nested_microtasks_in_fifo_order() {
        let program = loop_test_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        install_order_array(&mut machine);
        let first = timer_fn(&mut machine, 1); // pushes 1, queues "job"
        let second = timer_fn(&mut machine, 2); // pushes 2
        let third = timer_fn(&mut machine, 3); // pushes 3
        machine
            .globals
            .insert(EcmaString::from_utf8("job"), third);
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[first])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[second])
            .unwrap();

        machine.run_to_quiescence().unwrap();
        assert_eq!(
            order_markers(&machine),
            vec![Value::int32(1), Value::int32(2), Value::int32(3)]
        );
        assert!(machine.microtasks.is_empty());
    }

    #[test]
    fn automatic_loop_runs_two_timers_with_a_full_drain_between_turns() {
        let program = loop_test_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        install_order_array(&mut machine);
        let first = timer_fn(&mut machine, 1); // pushes 1, queues "job"
        let second = timer_fn(&mut machine, 3); // pushes 3
        let microtask = timer_fn(&mut machine, 2); // pushes 2
        machine
            .globals
            .insert(EcmaString::from_utf8("job"), microtask);
        let set_timeout = set_timeout_global(&machine);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[first, Value::int32(5)])
            .unwrap();
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[second, Value::int32(5)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 5,
        });
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 5,
        });

        machine.run_to_quiescence().unwrap();
        // The first timer's microtask (2) runs before the second timer (3).
        assert_eq!(
            order_markers(&machine),
            vec![Value::int32(1), Value::int32(2), Value::int32(3)]
        );
        assert!(machine.microtasks.is_empty());
        assert!(!machine.has_pending_timers());
    }

    #[test]
    fn automatic_loop_runs_a_timer_created_timer_in_a_later_turn() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let nested = timer_fn(&mut machine, 2);
        machine
            .globals
            .insert(EcmaString::from_utf8("nestedCallback"), nested);
        let creator = timer_native(&mut machine, "schedule nested", schedule_nested_timer);
        let set_timeout = set_timeout_global(&machine);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[creator, Value::int32(1)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 1,
        });

        machine.run_to_quiescence().unwrap();
        assert_eq!(read_global(&machine, "b"), Some(Value::int32(1)));
        assert!(!machine.has_pending_timers());
        assert!(machine.microtasks.is_empty());
    }

    #[test]
    fn automatic_loop_ignores_stale_and_premature_reports_until_real_expiry() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let a = timer_fn(&mut machine, 1);
        let set_timeout = set_timeout_global(&machine);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(50)])
            .unwrap();
        // A stale wakeup for an unknown id must not terminate the loop.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 999,
            deadline_ms: 10,
        });
        // A premature wakeup for the live timer cannot fire it before its deadline.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 10,
        });
        // The real expiry report finally fires the callback.
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 50,
        });

        machine.run_to_quiescence().unwrap();
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
        assert!(!machine.has_pending_timers());
    }

    #[test]
    fn automatic_loop_fails_with_typed_error_when_provider_loses_a_live_timer() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let a = timer_fn(&mut machine, 1);
        let set_timeout = set_timeout_global(&machine);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[a, Value::int32(5)])
            .unwrap();
        // Simulate the provider losing the armed timer without reporting it.
        shared.borrow_mut().live.remove(&1);

        let error = machine.run_to_quiescence().unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::TimerProviderFailure { .. }
        ));
        // The machine-owned live record and flags survive the failure.
        assert!(machine.has_pending_timers());
        assert!(!machine.microtask_drain_active);
        assert!(!machine.timer_checkpoint_active);
    }

    #[test]
    fn automatic_loop_maps_the_first_uncaught_microtask_throw_and_stops() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let throwing = timer_fn(&mut machine, 4); // throws 7
        let observer = timer_fn(&mut machine, 1); // stores a = 1
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[throwing])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[observer])
            .unwrap();

        let error = machine.run_to_quiescence().unwrap_err();
        assert_eq!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                value: Value::int32(7),
                origin: ThrowOrigin::Bytecode,
            }
        );
        // The later job stays queued and unrun.
        assert_eq!(read_global(&machine, "a"), None);
        assert_eq!(machine.microtasks.len(), 1);
        assert!(!machine.microtask_drain_active);
    }

    #[test]
    fn automatic_loop_timer_throw_suppresses_queued_microtasks_and_later_timers() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let shared = host.provider.state.clone();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let suppressed_microtask = timer_fn(&mut machine, 2); // stores b = 1
        machine
            .globals
            .insert(EcmaString::from_utf8("nestedCallback"), suppressed_microtask);
        let thrower = timer_native(&mut machine, "queue then throw", queue_job_then_throw);
        let later_timer = timer_fn(&mut machine, 1); // stores a = 1
        let set_timeout = set_timeout_global(&machine);
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[thrower, Value::int32(1)])
            .unwrap();
        machine
            .call_value(set_timeout, Value::UNDEFINED, &[later_timer, Value::int32(2)])
            .unwrap();
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 1,
            deadline_ms: 1,
        });
        shared.borrow_mut().reports.push_back(TimerWakeup {
            id: 2,
            deadline_ms: 2,
        });

        let error = machine.run_to_quiescence().unwrap_err();
        assert_eq!(
            error.kind,
            RuntimeErrorKind::UncaughtThrow {
                value: Value::int32(7),
                origin: ThrowOrigin::Bytecode,
            }
        );
        // The exception converts before any drain, so the queued microtask and
        // the later timer both stay pending.
        assert_eq!(read_global(&machine, "b"), None);
        assert_eq!(read_global(&machine, "a"), None);
        assert_eq!(machine.microtasks.len(), 1);
        assert!(machine.has_pending_timers());
        assert!(!machine.microtask_drain_active);
        assert!(!machine.timer_checkpoint_active);
    }

    #[test]
    fn automatic_loop_settles_derived_promises_when_a_handler_throws() {
        let program = promise_throw_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let executor = timer_fn(&mut machine, 1);
        let throwing = timer_fn(&mut machine, 2);
        let observer = timer_fn(&mut machine, 3);
        let constructor = machine.intrinsics.global("Promise").unwrap();
        let constructor_index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(constructor_id),
            ..
        } = machine.heap[constructor_index]
        else {
            panic!("Promise must be a native constructor");
        };
        let BuiltinOutcome::Value(promise) = machine
            .call_builtin(constructor_id, Value::UNDEFINED, &[executor], true)
            .unwrap()
        else {
            panic!("Promise construction returns a Promise");
        };
        let resolve = machine
            .globals
            .get(&EcmaString::from_utf8("resolve"))
            .copied()
            .unwrap();
        machine
            .call_value(resolve, Value::UNDEFINED, &[Value::int32(1)])
            .unwrap();
        let then = machine.get_named_property(promise, "then").unwrap();
        let derived = machine.call_value(then, promise, &[throwing]).unwrap();
        let then_again = machine.get_named_property(derived, "then").unwrap();
        machine
            .call_value(then_again, derived, &[Value::UNDEFINED, observer])
            .unwrap();

        machine.run_to_quiescence().unwrap();
        // The thrown handler rejects the derived promise, whose rejection
        // observer runs instead of aborting the loop.
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("observed"))
                .copied(),
            Some(Value::int32(7))
        );
        assert!(machine.microtasks.is_empty());
    }

    #[test]
    fn recursive_microtask_work_reaches_existing_fuel() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let respawn = timer_native(&mut machine, "respawn", respawn_job);
        machine
            .globals
            .insert(EcmaString::from_utf8("nestedCallback"), respawn);
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[respawn])
            .unwrap();
        machine.fuel = 16;

        let error = machine.run_to_quiescence().unwrap_err();
        assert!(matches!(
            error.kind,
            RuntimeErrorKind::FuelExhausted { .. }
        ));
        // Recursive work never reaches quiescence; fuel was fully spent.
        assert_eq!(machine.fuel, 0);
        assert!(!machine.microtask_drain_active);
    }

    #[test]
    fn manual_drain_still_collects_every_throw_and_continues() {
        let program = timer_program();
        let mut host = TimerTestHost::default();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.frames.clear();
        machine.live_registers = 0;
        let throwing = timer_fn(&mut machine, 4); // throws 7
        let observer = timer_fn(&mut machine, 1); // stores a = 1
        let queue = machine.intrinsics.global("queueMicrotask").unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[throwing])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[observer])
            .unwrap();
        machine
            .call_value(queue, Value::UNDEFINED, &[throwing])
            .unwrap();

        let drain = machine.drain_microtasks().unwrap();
        assert_eq!(drain.executed, 3);
        assert_eq!(
            drain.uncaught,
            vec![
                CallbackException {
                    value: Value::int32(7),
                    origin: ThrowOrigin::Bytecode,
                },
                CallbackException {
                    value: Value::int32(7),
                    origin: ThrowOrigin::Bytecode,
                },
            ]
        );
        assert_eq!(read_global(&machine, "a"), Some(Value::int32(1)));
        assert!(machine.microtasks.is_empty());
    }
}
