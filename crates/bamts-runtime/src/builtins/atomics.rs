//! SharedArrayBuffer and the synchronous Atomics operations.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::arraybuffer::{SharedBlock, Waiter, WaiterListHandle, WaiterState};
use super::typedarray_all::{
    ElementKind, TypedArraySnapshot, ViewBuffer, storage_from_value, typed_array_snapshot,
    value_from_storage,
};
use super::{
    allocate_string, builtin_property, define_data, heap_index, install_function, range_error,
    type_error,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    install_shared_array_buffer(heap, globals, builtins);
    install_atomics(heap, globals, builtins);
}

fn install_shared_array_buffer<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let constructor = install_function(
        heap,
        builtins,
        "SharedArrayBuffer",
        1,
        shared_array_buffer_constructor::<H>,
    );
    builtins.set_constructor_prototype(heap, constructor, prototype);
    builtins.set_sharedarraybuffer_prototype(prototype);
    define_data(heap, prototype, "constructor", constructor);
    let grow = install_function(heap, builtins, "grow", 1, grow::<H>);
    define_data(heap, prototype, "grow", grow);
    for (property_name, function_name, handler) in [
        (
            "byteLength",
            "get byteLength",
            sab_byte_length::<H> as BuiltinHandler<H>,
        ),
        (
            "maxByteLength",
            "get maxByteLength",
            sab_max_byte_length::<H>,
        ),
        ("growable", "get growable", sab_growable::<H>),
    ] {
        let getter = install_function(heap, builtins, function_name, 0, handler);
        let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
            unreachable!("SharedArrayBuffer.prototype is ordinary")
        };
        properties.insert(
            PropertyKey::Named(EcmaString::from_utf8(property_name)),
            Property::Accessor {
                getter: Some(getter),
                setter: None,
                enumerable: false,
                configurable: true,
            },
        );
    }
    let species = install_function(heap, builtins, "get [Symbol.species]", 0, species::<H>);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(constructor)] else {
        unreachable!("SharedArrayBuffer constructor is native")
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(builtins.symbol_species()) as u32),
        Property::Accessor {
            getter: Some(species),
            setter: None,
            enumerable: false,
            configurable: true,
        },
    );
    let tag = super::super::push(
        heap,
        HeapEntry::String(EcmaString::from_utf8("SharedArrayBuffer")),
    );
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!("SharedArrayBuffer.prototype is ordinary")
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(builtins.symbol_to_string_tag()) as u32),
        builtin_property(tag),
    );
    globals.insert(EcmaString::from_utf8("SharedArrayBuffer"), constructor);
}

fn install_atomics<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let atomics = super::super::ordinary_prototype(heap, builtins.object_prototype());
    for (name, length, handler) in [
        ("load", 2, load::<H> as BuiltinHandler<H>),
        ("store", 3, store::<H>),
        ("add", 3, add::<H>),
        ("sub", 3, sub::<H>),
        ("and", 3, bit_and::<H>),
        ("or", 3, bit_or::<H>),
        ("xor", 3, bit_xor::<H>),
        ("exchange", 3, exchange::<H>),
        ("compareExchange", 4, compare_exchange::<H>),
        ("isLockFree", 1, is_lock_free::<H>),
        ("wait", 4, wait::<H>),
        ("waitAsync", 4, wait_async::<H>),
        ("notify", 3, notify::<H>),
        ("pause", 0, pause::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, atomics, name, function);
    }
    let tag = super::super::push(heap, HeapEntry::String(EcmaString::from_utf8("Atomics")));
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(atomics)] else {
        unreachable!("Atomics is ordinary")
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(builtins.symbol_to_string_tag()) as u32),
        builtin_property(tag),
    );
    globals.insert(EcmaString::from_utf8("Atomics"), atomics);
}

fn shared_array_buffer_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _callee: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(type_error("SharedArrayBuffer constructor requires 'new'"));
    }
    let byte_length =
        super::arraybuffer::to_index(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let max_byte_length = match args.get(1).copied() {
        None | Some(Value::UNDEFINED) => None,
        Some(options) if !machine.is_object(options) => None,
        Some(options) => {
            let maximum = machine.get_named_property(options, "maxByteLength")?;
            if maximum == Value::UNDEFINED {
                None
            } else {
                Some(super::arraybuffer::to_index(machine, maximum)?)
            }
        }
    };
    if max_byte_length.is_some_and(|maximum| byte_length > maximum) {
        return Err(range_error(
            "SharedArrayBuffer byteLength exceeds maxByteLength",
        ));
    }
    machine
        .ensure_allocation_capacity(1, byte_length.saturating_add(1))
        .map_err(EvalFailure::Runtime)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_length)
        .map_err(|_| range_error("SharedArrayBuffer allocation failed"))?;
    bytes.resize(byte_length, 0);
    let intrinsic = machine.intrinsics.builtins.sharedarraybuffer_prototype();
    let new_target = machine.current_new_target();
    let prototype = if new_target == Value::UNDEFINED {
        intrinsic
    } else {
        let candidate = machine.get_named_property(new_target, "prototype")?;
        if machine.is_object(candidate) {
            candidate
        } else {
            intrinsic
        }
    };
    let value = machine
        .allocate(HeapEntry::SharedArrayBuffer {
            data: Arc::new(SharedBlock::new(bytes, max_byte_length)),
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(value))
}

fn shared_block<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Result<Arc<SharedBlock>, EvalFailure> {
    let Some(index) = machine.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
        return Err(type_error(
            "SharedArrayBuffer method called on incompatible receiver",
        ));
    };
    let HeapEntry::SharedArrayBuffer { data, .. } = &machine.heap[index] else {
        return Err(type_error(
            "SharedArrayBuffer method called on incompatible receiver",
        ));
    };
    Ok(Arc::clone(data))
}

fn sab_byte_length<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(crate::number_value(
        shared_block(machine, this)?.byte_length() as f64,
    )))
}

fn sab_max_byte_length<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(crate::number_value(
        shared_block(machine, this)?.max_byte_length() as f64,
    )))
}

fn sab_growable<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(Value::boolean(
        shared_block(machine, this)?.is_growable(),
    )))
}

fn grow<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let block = shared_block(machine, this)?;
    let length =
        super::arraybuffer::to_index(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    block
        .grow(length)
        .map_err(|()| range_error("Invalid SharedArrayBuffer grow length"))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn species<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(this))
}

#[derive(Clone, Copy)]
enum AtomicOperation {
    Load,
    Store,
    Add,
    Sub,
    And,
    Or,
    Xor,
    Exchange,
    CompareExchange,
}

fn integer_snapshot<H: Host>(
    machine: &mut Machine<'_, H>,
    view: Value,
    index_value: Value,
) -> Result<(TypedArraySnapshot, usize), EvalFailure> {
    let snapshot = typed_array_snapshot(machine, view)?;
    if snapshot.bounds.detached || snapshot.bounds.out_of_bounds {
        return Err(type_error(
            "Atomics operation requires an in-bounds TypedArray",
        ));
    }
    if matches!(
        snapshot.kind,
        ElementKind::Uint8Clamped
            | ElementKind::Float16
            | ElementKind::Float32
            | ElementKind::Float64
    ) {
        return Err(type_error(
            "Atomics operation requires an integer TypedArray",
        ));
    }
    let index = super::arraybuffer::to_index(machine, index_value)?;
    if index >= snapshot.bounds.element_length {
        return Err(range_error("Atomics index is outside the TypedArray"));
    }
    Ok((snapshot, index))
}

fn atomic<H: Host>(
    machine: &mut Machine<'_, H>,
    args: &[Value],
    operation: AtomicOperation,
) -> Result<BuiltinOutcome, EvalFailure> {
    let view = args.first().copied().unwrap_or(Value::UNDEFINED);
    let (snapshot, index) = integer_snapshot(
        machine,
        view,
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    let size = snapshot.kind.element_size();
    let replacement = match operation {
        AtomicOperation::Load => None,
        AtomicOperation::CompareExchange => Some(storage_from_value(
            machine,
            snapshot.kind,
            args.get(3).copied().unwrap_or(Value::UNDEFINED),
        )?),
        _ => Some(storage_from_value(
            machine,
            snapshot.kind,
            args.get(2).copied().unwrap_or(Value::UNDEFINED),
        )?),
    };
    let expected = if matches!(operation, AtomicOperation::CompareExchange) {
        Some(storage_from_value(
            machine,
            snapshot.kind,
            args.get(2).copied().unwrap_or(Value::UNDEFINED),
        )?)
    } else {
        None
    };
    let start = snapshot.byte_offset + index * size;
    let mut old = [0_u8; 8];
    snapshot.buffer.with_bytes_mut(machine, |bytes| {
        old[..size].copy_from_slice(&bytes[start..start + size]);
        if let Some(replacement) = replacement {
            let old_raw = raw(&old, size);
            let argument = raw(&replacement, size);
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1_u64 << (size * 8)) - 1
            };
            let next = match operation {
                AtomicOperation::Store | AtomicOperation::Exchange => argument,
                AtomicOperation::Add => old_raw.wrapping_add(argument) & mask,
                AtomicOperation::Sub => old_raw.wrapping_sub(argument) & mask,
                AtomicOperation::And => old_raw & argument,
                AtomicOperation::Or => old_raw | argument,
                AtomicOperation::Xor => old_raw ^ argument,
                AtomicOperation::CompareExchange => {
                    if old[..size] == expected.unwrap()[..size] {
                        argument
                    } else {
                        old_raw
                    }
                }
                AtomicOperation::Load => old_raw,
            };
            bytes[start..start + size].copy_from_slice(&next.to_le_bytes()[..size]);
        }
    })?;
    let result = if matches!(operation, AtomicOperation::Store) {
        value_from_storage(machine, snapshot.kind, replacement.unwrap())?
    } else {
        value_from_storage(machine, snapshot.kind, old)?
    };
    Ok(BuiltinOutcome::Value(result))
}

fn raw(storage: &[u8; 8], size: usize) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes[..size].copy_from_slice(&storage[..size]);
    u64::from_le_bytes(bytes)
}

macro_rules! atomics_handlers {
    ($(($name:ident, $operation:expr)),* $(,)?) => {$ (
        fn $name<H: Host>(
            machine: &mut Machine<'_, H>, _this: Value, args: &[Value], _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> { atomic(machine, args, $operation) }
    )* };
}

atomics_handlers!(
    (load, AtomicOperation::Load),
    (store, AtomicOperation::Store),
    (add, AtomicOperation::Add),
    (sub, AtomicOperation::Sub),
    (bit_and, AtomicOperation::And),
    (bit_or, AtomicOperation::Or),
    (bit_xor, AtomicOperation::Xor),
    (exchange, AtomicOperation::Exchange),
    (compare_exchange, AtomicOperation::CompareExchange),
);

fn waitable_snapshot<H: Host>(
    machine: &mut Machine<'_, H>,
    args: &[Value],
) -> Result<(TypedArraySnapshot, usize), EvalFailure> {
    let result = integer_snapshot(
        machine,
        args.first().copied().unwrap_or(Value::UNDEFINED),
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    if !matches!(result.0.kind, ElementKind::Int32 | ElementKind::BigInt64) {
        return Err(type_error(
            "Atomics.wait requires an Int32Array or BigInt64Array",
        ));
    }
    Ok(result)
}

fn waiter_list<H: Host>(
    machine: &Machine<'_, H>,
    buffer: &ViewBuffer,
) -> Result<WaiterListHandle, EvalFailure> {
    match buffer {
        ViewBuffer::Array(buffer) => buffer.waiter_list(machine),
        ViewBuffer::Shared(buffer) => Ok(buffer.waiter_list()),
    }
}

fn wait_result<H: Host>(
    machine: &mut Machine<'_, H>,
    text: &str,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::from_utf8(text),
    )?))
}

fn wait<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !machine.limits.agent_can_suspend {
        return Err(type_error("Atomics.wait cannot suspend this agent"));
    }
    let (snapshot, index) = waitable_snapshot(machine, args)?;
    let expected = storage_from_value(
        machine,
        snapshot.kind,
        args.get(2).copied().unwrap_or(Value::UNDEFINED),
    )?;
    let timeout = match args.get(3).copied() {
        None | Some(Value::UNDEFINED) => None,
        Some(value) => {
            let number = match machine.coerce_number_observable(value)?.decode() {
                Some(Decoded::Int32(value)) => f64::from(value as i32),
                Some(Decoded::Number(value)) => value,
                _ => unreachable!("ToNumber returns a number"),
            };
            if number.is_nan() || number == f64::INFINITY {
                None
            } else {
                Some(Duration::from_secs_f64(number.max(0.0) / 1000.0))
            }
        }
    };
    let byte_position = snapshot.byte_offset + index * snapshot.kind.element_size();
    let waiters = waiter_list(machine, &snapshot.buffer)?;
    let mut lists = waiters
        .lists
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let equal = snapshot.buffer.with_bytes(machine, |bytes| {
        bytes[byte_position..byte_position + snapshot.kind.element_size()]
            == expected[..snapshot.kind.element_size()]
    })?;
    if !equal {
        drop(lists);
        return wait_result(machine, "not-equal");
    }
    let waiter = Waiter::handle();
    lists
        .entry(byte_position)
        .or_default()
        .push_back(Arc::clone(&waiter));
    drop(lists);

    let state = waiter
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let notified = if let Some(timeout) = timeout {
        let (state, _) = waiter
            .ready
            .wait_timeout_while(state, timeout, |state| *state == WaiterState::Waiting)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state == WaiterState::Notified
    } else {
        let state = waiter
            .ready
            .wait_while(state, |state| *state == WaiterState::Waiting)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state == WaiterState::Notified
    };
    if !notified {
        let mut lists = waiters
            .lists
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(queue) = lists.get_mut(&byte_position) {
            queue.retain(|candidate| !Arc::ptr_eq(candidate, &waiter));
            if queue.is_empty() {
                lists.remove(&byte_position);
            }
        }
    }
    wait_result(machine, if notified { "ok" } else { "timed-out" })
}

fn wait_async_result<H: Host>(
    machine: &mut Machine<'_, H>,
    asynchronous: bool,
    value: Value,
) -> Result<BuiltinOutcome, EvalFailure> {
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.builtins.object_prototype()),
            boxed_primitive: None,
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    machine.set_data_property(object, "async", Value::boolean(asynchronous))?;
    machine.set_data_property(object, "value", value)?;
    Ok(BuiltinOutcome::Value(object))
}

fn wait_async<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (snapshot, index) = waitable_snapshot(machine, args)?;
    let expected = storage_from_value(
        machine,
        snapshot.kind,
        args.get(2).copied().unwrap_or(Value::UNDEFINED),
    )?;
    let timeout = match args.get(3).copied() {
        None | Some(Value::UNDEFINED) => None,
        Some(value) => {
            let number = match machine.coerce_number_observable(value)?.decode() {
                Some(Decoded::Int32(value)) => f64::from(value as i32),
                Some(Decoded::Number(value)) => value,
                _ => unreachable!("ToNumber returns a number"),
            };
            if number.is_nan() || number == f64::INFINITY {
                None
            } else {
                Some(Duration::from_secs_f64(number.max(0.0) / 1000.0))
            }
        }
    };
    let byte_position = snapshot.byte_offset + index * snapshot.kind.element_size();
    let waiters = waiter_list(machine, &snapshot.buffer)?;
    let mut lists = waiters
        .lists
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let equal = snapshot.buffer.with_bytes(machine, |bytes| {
        bytes[byte_position..byte_position + snapshot.kind.element_size()]
            == expected[..snapshot.kind.element_size()]
    })?;
    if !equal {
        drop(lists);
        let value = allocate_string(machine, EcmaString::from_utf8("not-equal"))?;
        return wait_async_result(machine, false, value);
    }
    let waiter = Waiter::handle();
    lists
        .entry(byte_position)
        .or_default()
        .push_back(Arc::clone(&waiter));
    drop(lists);
    let promise = machine.create_promise()?;
    let promise_bits = promise.to_bits();
    let inbox = machine.external_inbox();
    inbox.reserve();
    std::thread::spawn({
        let waiters = Arc::clone(&waiters);
        move || {
            let state = waiter
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let notified = if let Some(timeout) = timeout {
                let (state, _) = waiter
                    .ready
                    .wait_timeout_while(state, timeout, |state| *state == WaiterState::Waiting)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *state == WaiterState::Notified
            } else {
                let state = waiter
                    .ready
                    .wait_while(state, |state| *state == WaiterState::Waiting)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *state == WaiterState::Notified
            };
            if !notified {
                let mut lists = waiters
                    .lists
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(queue) = lists.get_mut(&byte_position) {
                    queue.retain(|candidate| !Arc::ptr_eq(candidate, &waiter));
                    if queue.is_empty() {
                        lists.remove(&byte_position);
                    }
                }
            }
            inbox.push(crate::ExternalJob::ResolveWaitAsync {
                promise: promise_bits,
                result: if notified { "ok" } else { "timed-out" },
            });
        }
    });
    wait_async_result(machine, true, promise)
}

fn notify<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (snapshot, index) = waitable_snapshot(machine, args)?;
    let count = match args.get(2).copied() {
        None | Some(Value::UNDEFINED) => usize::MAX,
        Some(value) => super::arraybuffer::to_index(machine, value)?,
    };
    let byte_position = snapshot.byte_offset + index * snapshot.kind.element_size();
    let waiters = waiter_list(machine, &snapshot.buffer)?;
    let mut lists = waiters
        .lists
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut woken = Vec::new();
    if let Some(queue) = lists.get_mut(&byte_position) {
        for _ in 0..count {
            let Some(waiter) = queue.pop_front() else {
                break;
            };
            woken.push(waiter);
        }
        if queue.is_empty() {
            lists.remove(&byte_position);
        }
    }
    drop(lists);
    for waiter in &woken {
        *waiter
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = WaiterState::Notified;
        waiter.ready.notify_one();
    }
    Ok(BuiltinOutcome::Value(crate::number_value(
        woken.len() as f64
    )))
}

fn is_lock_free<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let size =
        super::arraybuffer::to_index(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(matches!(
        size,
        1 | 2 | 4 | 8
    ))))
}

fn pause<H: Host>(
    _machine: &mut Machine<'_, H>,
    _this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TestHost, blank_program};
    use super::*;
    use crate::{Limits, PromiseState};

    fn shared_int32(machine: &mut Machine<'_, TestHost>) -> Value {
        let buffer_constructor = machine
            .intrinsics
            .global("SharedArrayBuffer")
            .expect("SharedArrayBuffer is installed");
        let buffer = machine
            .construct_value(buffer_constructor, &[Value::int32(4)])
            .expect("SharedArrayBuffer construction succeeds");
        let view_constructor = machine
            .intrinsics
            .global("Int32Array")
            .expect("Int32Array is installed");
        machine
            .construct_value(view_constructor, &[buffer])
            .expect("Int32Array construction succeeds")
    }

    fn promise_state(machine: &Machine<'_, TestHost>, promise: Value) -> PromiseState {
        let index = machine.runtime_slot(promise).unwrap().unwrap();
        let HeapEntry::Promise { state, .. } = &machine.heap[index] else {
            panic!("waitAsync value is a Promise");
        };
        state.clone()
    }

    #[test]
    fn wait_async_reports_mismatch_without_a_promise() {
        let program = blank_program("Atomics.waitAsync mismatch");
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let view = shared_int32(&mut machine);
        let BuiltinOutcome::Value(result) = wait_async(
            &mut machine,
            Value::UNDEFINED,
            &[view, Value::int32(0), Value::int32(1)],
            false,
        )
        .unwrap() else {
            panic!("waitAsync returns a result object");
        };
        assert_eq!(
            machine.get_named_property(result, "async").unwrap(),
            Value::FALSE
        );
        let value = machine.get_named_property(result, "value").unwrap();
        assert!(
            machine
                .string_value(value)
                .is_some_and(|text| text.eq_ascii("not-equal"))
        );
    }

    #[test]
    fn wait_async_resolves_after_notify_through_the_external_job_queue() {
        let program = blank_program("Atomics.waitAsync notify");
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let view = shared_int32(&mut machine);
        let BuiltinOutcome::Value(result) = wait_async(
            &mut machine,
            Value::UNDEFINED,
            &[
                view,
                Value::int32(0),
                Value::int32(0),
                crate::number_value(1000.0),
            ],
            false,
        )
        .unwrap() else {
            panic!("waitAsync returns a result object");
        };
        assert_eq!(
            machine.get_named_property(result, "async").unwrap(),
            Value::TRUE
        );
        let promise = machine.get_named_property(result, "value").unwrap();
        assert!(matches!(
            promise_state(&machine, promise),
            PromiseState::Pending { .. }
        ));
        let BuiltinOutcome::Value(count) = notify(
            &mut machine,
            Value::UNDEFINED,
            &[view, Value::int32(0), Value::int32(1)],
            false,
        )
        .unwrap() else {
            panic!("Atomics.notify returns a count");
        };
        assert_eq!(count, crate::number_value(1.0));
        machine.run_to_quiescence().unwrap();
        let PromiseState::Fulfilled { value } = promise_state(&machine, promise) else {
            panic!("waitAsync promise was not fulfilled");
        };
        assert!(
            machine
                .string_value(value)
                .is_some_and(|text| text.eq_ascii("ok"))
        );
    }
}
