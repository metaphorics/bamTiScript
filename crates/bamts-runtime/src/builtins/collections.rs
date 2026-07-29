use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, builtin_property, define_data, heap_index, install_function, type_error,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{
    EvalFailure, HeapEntry, Host, IterationKind, Machine, Property, PropertyKey, PropertyMap,
};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    install_map(heap, globals, builtins);
    install_set(heap, globals, builtins);
    install_weak_map(heap, globals, builtins);
    install_weak_set(heap, globals, builtins);
}

fn install_map<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.object_prototype()));
    let constructor = install_function(heap, builtins, "Map", 0, map_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("set", 2, map_set::<H> as BuiltinHandler<H>),
        ("get", 1, map_get::<H>),
        ("has", 1, map_has::<H>),
        ("delete", 1, map_delete::<H>),
        ("clear", 0, map_clear::<H>),
        ("keys", 0, map_keys::<H>),
        ("values", 0, map_values::<H>),
        ("entries", 0, map_entries::<H>),
        ("forEach", 1, map_for_each::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
    }
    let size = install_function(heap, builtins, "get size", 0, map_size::<H>);
    define_getter(heap, prototype, "size", size);
    let entries = named_property(heap, prototype, "entries");
    define_symbol(heap, prototype, builtins.symbol_iterator(), entries);
    globals.insert(EcmaString::from_utf8("Map"), constructor);
}

fn install_set<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.object_prototype()));
    let constructor = install_function(heap, builtins, "Set", 0, set_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("add", 1, set_add::<H> as BuiltinHandler<H>),
        ("has", 1, set_has::<H>),
        ("delete", 1, set_delete::<H>),
        ("clear", 0, set_clear::<H>),
        ("keys", 0, set_values::<H>),
        ("values", 0, set_values::<H>),
        ("entries", 0, set_entries::<H>),
        ("forEach", 1, set_for_each::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
    }
    let size = install_function(heap, builtins, "get size", 0, set_size::<H>);
    define_getter(heap, prototype, "size", size);
    let values = named_property(heap, prototype, "values");
    define_symbol(heap, prototype, builtins.symbol_iterator(), values);
    globals.insert(EcmaString::from_utf8("Set"), constructor);
}

fn install_weak_map<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.object_prototype()));
    let constructor = install_function(heap, builtins, "WeakMap", 0, weak_map_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("set", 2, weak_map_set::<H> as BuiltinHandler<H>),
        ("get", 1, map_get::<H>),
        ("has", 1, map_has::<H>),
        ("delete", 1, map_delete::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
    }
    globals.insert(EcmaString::from_utf8("WeakMap"), constructor);
}

fn install_weak_set<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.object_prototype()));
    let constructor = install_function(heap, builtins, "WeakSet", 0, weak_set_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("add", 1, weak_set_add::<H> as BuiltinHandler<H>),
        ("has", 1, set_has::<H>),
        ("delete", 1, set_delete::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
    }
    globals.insert(EcmaString::from_utf8("WeakSet"), constructor);
}

pub(super) fn install_iterator_prototype<H: Host>(
    heap: &mut Vec<HeapEntry>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.object_prototype()));
    let next = install_function(heap, builtins, "next", 0, iterator_next::<H>);
    let identity = install_function(
        heap,
        builtins,
        "[Symbol.iterator]",
        0,
        iterator_identity::<H>,
    );
    define_data(heap, prototype, "next", next);
    define_symbol(heap, prototype, builtins.symbol_iterator(), identity);
    builtins.set_iterator_prototype(prototype);
}

pub(super) fn install_generator_prototype<H: Host>(
    heap: &mut Vec<HeapEntry>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = ordinary(heap, Some(builtins.iterator_prototype()));
    let next = install_function(heap, builtins, "next", 1, generator_next::<H>);
    define_data(heap, prototype, "next", next);
    builtins.set_generator_prototype(prototype);
}

fn generator_next<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::GeneratorNext {
        generator: this,
        resume_value: args.first().copied().unwrap_or(Value::UNDEFINED),
    })
}

pub(super) fn iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    source: Value,
    kind: IterationKind,
) -> Result<Value, EvalFailure> {
    machine
        .allocate(HeapEntry::BuiltinIterator {
            source,
            kind,
            position: Some(0),
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.builtins.iterator_prototype()),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)
}

fn iterator_identity<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(this))
}

fn iterator_next<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let Some(iterator_index) = machine.runtime_slot(this).map_err(EvalFailure::Runtime)? else {
        return Err(type_error("iterator next called on incompatible receiver"));
    };
    let HeapEntry::BuiltinIterator {
        source,
        kind,
        position,
        ..
    } = machine.heap[iterator_index]
    else {
        return Err(type_error("iterator next called on incompatible receiver"));
    };
    let (done, value, next_position) = match position {
        None => (true, Value::UNDEFINED, None),
        Some(cursor) => {
            let source_index = machine
                .runtime_slot(source)
                .map_err(EvalFailure::Runtime)?
                .ok_or_else(|| type_error("iterator next called on incompatible receiver"))?;
            let item = match &machine.heap[source_index] {
                HeapEntry::Array { elements, .. } => {
                    usize::try_from(cursor).ok().and_then(|index| {
                        elements.get(index).map(|element| {
                            let value = if *element == Value::HOLE {
                                Value::UNDEFINED
                            } else {
                                *element
                            };
                            let next = cursor
                                .checked_add(1)
                                .expect("array bounds keep iterator positions below u64::MAX");
                            (next, crate::number_value(index as f64), value)
                        })
                    })
                }
                HeapEntry::Collection { entries, .. } => {
                    let index = entries.partition_point(|entry| entry.order < cursor);
                    entries.get(index).map(|entry| {
                        let next = entry
                            .order
                            .checked_add(1)
                            .expect("heap limits keep collection order below u64::MAX");
                        (next, entry.key, entry.value)
                    })
                }
                _ => {
                    return Err(type_error("iterator next called on incompatible receiver"));
                }
            };
            match item {
                None => (true, Value::UNDEFINED, None),
                Some((next, key, item_value)) => {
                    let value = match kind {
                        IterationKind::Key => key,
                        IterationKind::Value => item_value,
                        IterationKind::Entry => allocate_array(machine, vec![key, item_value])?,
                    };
                    (false, value, Some(next))
                }
            }
        }
    };
    let HeapEntry::BuiltinIterator { position, .. } = &mut machine.heap[iterator_index] else {
        unreachable!("iterator brand was checked")
    };
    *position = next_position;
    let result = ordinary_runtime(machine, None)?;
    machine.set_data_property(result, "value", value)?;
    machine.set_data_property(result, "done", Value::boolean(done))?;
    Ok(BuiltinOutcome::Value(result))
}

fn map_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_like_constructor(machine, args, constructing, "Map")
}

fn set_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    set_like_constructor(machine, args, constructing, "Set")
}

fn weak_map_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_like_constructor(machine, args, constructing, "WeakMap")
}

fn weak_set_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    set_like_constructor(machine, args, constructing, "WeakSet")
}

fn map_like_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    args: &[Value],
    constructing: bool,
    name: &str,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(type_error("collection constructor requires 'new'"));
    }
    let object = collection(machine, constructor_prototype(machine, name)?)?;
    if let Some(source) = args
        .first()
        .copied()
        .filter(|value| !matches!(value.decode(), Some(Decoded::Null | Decoded::Undefined)))
    {
        let entries = machine.iterable_values(source)?;
        for entry in entries {
            if !machine.is_object(entry) {
                return Err(type_error("Iterator value is not an entry object"));
            }
            let key = machine.get_named_property(entry, "0")?;
            let value = machine.get_named_property(entry, "1")?;
            if name == "WeakMap" {
                require_weak_key(machine, key)?;
            }
            map_put(machine, object, key, value)?;
        }
    }
    Ok(BuiltinOutcome::Value(object))
}

fn set_like_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    args: &[Value],
    constructing: bool,
    name: &str,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(type_error("collection constructor requires 'new'"));
    }
    let object = collection(machine, constructor_prototype(machine, name)?)?;
    if let Some(source) = args
        .first()
        .copied()
        .filter(|value| !matches!(value.decode(), Some(Decoded::Null | Decoded::Undefined)))
    {
        let values = machine.iterable_values(source)?;
        for value in values {
            if name == "WeakSet" {
                require_weak_key(machine, value)?;
            }
            set_put(machine, object, value)?;
        }
    }
    Ok(BuiltinOutcome::Value(object))
}

fn map_set<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_put(
        machine,
        this,
        args.first().copied().unwrap_or(Value::UNDEFINED),
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    Ok(BuiltinOutcome::Value(this))
}
fn weak_map_set<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = args.first().copied().unwrap_or(Value::UNDEFINED);
    require_weak_key(machine, key)?;
    map_put(
        machine,
        this,
        key,
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    Ok(BuiltinOutcome::Value(this))
}
fn map_get<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let slot = collection_slot(machine, this)?;
    let key = args.first().copied().unwrap_or(Value::UNDEFINED);
    let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    let found = entries
        .iter()
        .find(|entry| machine.same_value_zero(entry.key, key))
        .map_or(Value::UNDEFINED, |entry| entry.value);
    Ok(BuiltinOutcome::Value(found))
}
fn map_has<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let slot = collection_slot(machine, this)?;
    let key = args.first().copied().unwrap_or(Value::UNDEFINED);
    let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    Ok(BuiltinOutcome::Value(Value::boolean(
        entries
            .iter()
            .any(|entry| machine.same_value_zero(entry.key, key)),
    )))
}
fn map_delete<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let slot = collection_slot(machine, this)?;
    let key = args.first().copied().unwrap_or(Value::UNDEFINED);
    let index = {
        let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
            unreachable!("collection brand was checked")
        };
        entries
            .iter()
            .position(|entry| machine.same_value_zero(entry.key, key))
    };
    let Some(index) = index else {
        return Ok(BuiltinOutcome::Value(Value::FALSE));
    };
    let HeapEntry::Collection { entries, .. } = &mut machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    entries.remove(index);
    Ok(BuiltinOutcome::Value(Value::TRUE))
}
fn map_clear<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let slot = collection_slot(machine, this)?;
    let HeapEntry::Collection { entries, .. } = &mut machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    entries.clear();
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}
fn map_size<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let slot = collection_slot(machine, this)?;
    let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    Ok(BuiltinOutcome::Value(crate::number_value(
        entries.len() as f64
    )))
}
fn map_keys<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_iterator(machine, this, IterationKind::Key)
}
fn map_values<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_iterator(machine, this, IterationKind::Value)
}
fn map_entries<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_iterator(machine, this, IterationKind::Entry)
}
fn map_for_each<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let callback = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !machine.is_callable(callback)? {
        return Err(type_error("Map.forEach callback is not callable"));
    }
    for (key, value) in collection_snapshot(machine, this)? {
        machine.call_value(
            callback,
            args.get(1).copied().unwrap_or(Value::UNDEFINED),
            &[value, key, this],
        )?;
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn set_add<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    set_put(
        machine,
        this,
        args.first().copied().unwrap_or(Value::UNDEFINED),
    )?;
    Ok(BuiltinOutcome::Value(this))
}
fn weak_set_add<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    require_weak_key(machine, value)?;
    set_put(machine, this, value)?;
    Ok(BuiltinOutcome::Value(this))
}
fn set_has<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_has(machine, this, args, constructing)
}
fn set_delete<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_delete(machine, this, args, constructing)
}
fn set_clear<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_clear(machine, this, args, constructing)
}
fn set_size<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    map_size(machine, this, args, constructing)
}
fn set_values<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_iterator(machine, this, IterationKind::Value)
}
fn set_entries<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_iterator(machine, this, IterationKind::Entry)
}
fn set_for_each<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let callback = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !machine.is_callable(callback)? {
        return Err(type_error("Set.forEach callback is not callable"));
    }
    for (_, value) in collection_snapshot(machine, this)? {
        machine.call_value(
            callback,
            args.get(1).copied().unwrap_or(Value::UNDEFINED),
            &[value, value, this],
        )?;
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn map_put<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    key: Value,
    value: Value,
) -> Result<(), EvalFailure> {
    let slot = collection_slot(machine, object)?;
    let existing = {
        let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
            unreachable!("collection brand was checked")
        };
        entries
            .iter()
            .position(|entry| machine.same_value_zero(entry.key, key))
    };
    if let Some(index) = existing {
        let HeapEntry::Collection { entries, .. } = &mut machine.heap[slot] else {
            unreachable!("collection brand was checked")
        };
        entries[index].value = value;
        return Ok(());
    }
    append_collection_entry(machine, slot, key, value)
}
fn set_put<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    value: Value,
) -> Result<(), EvalFailure> {
    let slot = collection_slot(machine, object)?;
    let exists = {
        let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
            unreachable!("collection brand was checked")
        };
        entries
            .iter()
            .any(|entry| machine.same_value_zero(entry.key, value))
    };
    if exists {
        return Ok(());
    }
    append_collection_entry(machine, slot, value, value)
}

pub(super) fn append_collection_entry<H: Host>(
    machine: &mut Machine<'_, H>,
    slot: usize,
    key: Value,
    value: Value,
) -> Result<(), EvalFailure> {
    let order = match machine.heap[slot] {
        HeapEntry::Collection { next_order, .. } => next_order,
        _ => unreachable!("collection slot owns collection storage"),
    };
    let next_order = order
        .checked_add(1)
        .expect("heap limits keep collection order below u64::MAX");
    machine
        .charge_heap(crate::CollectionEntry::BYTES)
        .map_err(EvalFailure::Runtime)?;
    let HeapEntry::Collection {
        entries,
        next_order: stored_next_order,
        ..
    } = &mut machine.heap[slot]
    else {
        unreachable!("collection slot owns collection storage")
    };
    entries.push(crate::CollectionEntry { order, key, value });
    *stored_next_order = next_order;
    Ok(())
}

fn collection<H: Host>(
    machine: &mut Machine<'_, H>,
    prototype: Value,
) -> Result<Value, EvalFailure> {
    machine
        .allocate(HeapEntry::Collection {
            entries: Vec::new(),
            next_order: 0,
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)
}
fn collection_slot<H: Host>(machine: &Machine<'_, H>, object: Value) -> Result<usize, EvalFailure> {
    let Some(index) = machine.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
        return Err(type_error(
            "collection method called on incompatible receiver",
        ));
    };
    if !matches!(machine.heap[index], HeapEntry::Collection { .. }) {
        return Err(type_error(
            "collection method called on incompatible receiver",
        ));
    }
    Ok(index)
}

fn collection_snapshot<H: Host>(
    machine: &Machine<'_, H>,
    object: Value,
) -> Result<Vec<(Value, Value)>, EvalFailure> {
    let slot = collection_slot(machine, object)?;
    let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
        unreachable!("collection brand was checked")
    };
    Ok(entries
        .iter()
        .map(|entry| (entry.key, entry.value))
        .collect())
}
fn collection_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    kind: IterationKind,
) -> Result<BuiltinOutcome, EvalFailure> {
    collection_slot(machine, object)?;
    Ok(BuiltinOutcome::Value(iterator(machine, object, kind)?))
}
fn require_weak_key<H: Host>(machine: &Machine<'_, H>, key: Value) -> Result<(), EvalFailure> {
    let Some(index) = machine.runtime_slot(key).map_err(EvalFailure::Runtime)? else {
        return Err(type_error("Invalid value used as weak collection key"));
    };
    if matches!(
        machine.heap[index],
        HeapEntry::String(_) | HeapEntry::BigInt(_)
    ) {
        return Err(type_error("Invalid value used as weak collection key"));
    }
    Ok(())
}

fn constructor_prototype<H: Host>(
    machine: &Machine<'_, H>,
    name: &str,
) -> Result<Value, EvalFailure> {
    let constructor = machine
        .intrinsics
        .global(name)
        .ok_or_else(|| type_error("missing collection constructor"))?;
    let index = machine
        .runtime_slot(constructor)
        .map_err(EvalFailure::Runtime)?
        .ok_or_else(|| type_error("invalid collection constructor"))?;
    let HeapEntry::NativeFunction { properties, .. } = &machine.heap[index] else {
        return Err(type_error("invalid collection constructor"));
    };
    match properties.get(&PropertyKey::Named(EcmaString::from_utf8("prototype"))) {
        Some(Property::Data { value, .. }) => Ok(*value),
        _ => Err(type_error("missing collection prototype")),
    }
}
fn ordinary(heap: &mut Vec<HeapEntry>, prototype: Option<Value>) -> Value {
    super::super::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype,
            extensible: true,
            boxed_primitive: None,
        },
    )
}
fn ordinary_runtime<H: Host>(
    machine: &mut Machine<'_, H>,
    prototype: Option<Value>,
) -> Result<Value, EvalFailure> {
    machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype,
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)
}
fn define_getter(heap: &mut [HeapEntry], object: Value, name: &str, getter: Value) {
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        Property::Accessor {
            getter: Some(getter),
            setter: None,
            enumerable: false,
            configurable: true,
        },
    );
}
fn define_symbol(heap: &mut [HeapEntry], object: Value, symbol: Value, value: Value) {
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(symbol) as u32),
        builtin_property(value),
    );
}
fn named_property(heap: &[HeapEntry], object: Value, name: &str) -> Value {
    let HeapEntry::Object { properties, .. } = &heap[heap_index(object)] else {
        unreachable!()
    };
    match properties.get(&PropertyKey::Named(EcmaString::from_utf8(name))) {
        Some(Property::Data { value, .. }) => *value,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Verified,
    };
    use bamts_native::Decoded;

    use super::*;
    use crate::intrinsics::BuiltinDef;
    use crate::{Limits, NativeCallable, ThrowOrigin};

    #[derive(Default)]
    struct TestHost;

    impl Host for TestHost {}

    fn module() -> Program<Verified> {
        let code = Module::new(
            vec![Constant::String(EcmaString::from_utf8("<test>"))],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![Instruction::Halt],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("valid test module");
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
        .expect("valid test program")
    }

    fn object(machine: &mut Machine<'_, TestHost>) -> Value {
        machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap()
    }

    fn construct_builtin(
        machine: &mut Machine<'_, TestHost>,
        name: &str,
        arguments: &[Value],
    ) -> Value {
        let constructor = machine.intrinsics.global(name).expect("global exists");
        let index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("constructor is native")
        };
        let BuiltinOutcome::Value(value) = machine
            .call_builtin(id, Value::UNDEFINED, arguments, true)
            .unwrap()
        else {
            panic!("constructor returns a value")
        };
        value
    }

    fn builtin_id(machine: &Machine<'_, TestHost>, name: &str) -> crate::intrinsics::BuiltinId {
        let constructor = machine.intrinsics.global(name).expect("global exists");
        let index = machine.runtime_slot(constructor).unwrap().unwrap();
        match machine.heap[index] {
            HeapEntry::NativeFunction {
                callable: NativeCallable::Builtin(id),
                ..
            } => id,
            _ => panic!("not a builtin constructor"),
        }
    }

    // ---- custom iterable helpers -------------------------------------------

    fn custom_iterator_next<H: Host>(
        machine: &mut Machine<'_, H>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let values = machine.get_named_property(this, "_values")?;
        let index_val = machine.get_named_property(this, "_index")?;
        let elements = machine.array_elements(values)?.unwrap_or_default();
        let index = match index_val.decode() {
            Some(Decoded::Int32(i)) => i as usize,
            Some(Decoded::Number(n)) => n as usize,
            _ => 0,
        };
        let result = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        if index >= elements.len() {
            machine.set_data_property(result, "done", Value::TRUE)?;
            machine.set_data_property(result, "value", Value::UNDEFINED)?;
        } else {
            machine.set_data_property(result, "done", Value::FALSE)?;
            machine.set_data_property(result, "value", elements[index])?;
            machine.set_data_property(this, "_index", Value::int32((index + 1) as u32))?;
        }
        Ok(BuiltinOutcome::Value(result))
    }

    fn custom_iterator_create<H: Host>(
        machine: &mut Machine<'_, H>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let iter = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        let values = machine.get_named_property(this, "_values")?;
        let next = machine.get_named_property(this, "_next")?;
        machine.set_data_property(iter, "_values", values)?;
        machine.set_data_property(iter, "_index", Value::int32(0))?;
        machine.set_data_property(iter, "next", next)?;
        Ok(BuiltinOutcome::Value(iter))
    }

    fn custom_iterable(machine: &mut Machine<'_, TestHost>, values: Vec<Value>) -> Value {
        let next_id = machine.intrinsics.builtins.register(BuiltinDef {
            name: "custom next",
            length: 0,
            handler: custom_iterator_next::<TestHost>,
        });
        let next_fn =
            crate::intrinsics::native_function(&mut machine.heap, next_id, "custom next", 0);
        let create_id = machine.intrinsics.builtins.register(BuiltinDef {
            name: "custom iterator",
            length: 0,
            handler: custom_iterator_create::<TestHost>,
        });
        let create_fn =
            crate::intrinsics::native_function(&mut machine.heap, create_id, "custom iterator", 0);
        let iterable = object(machine);
        let values_array = allocate_array(machine, values).unwrap();
        machine
            .set_data_property(iterable, "_values", values_array)
            .unwrap();
        machine
            .set_data_property(iterable, "_next", next_fn)
            .unwrap();
        let iterator_symbol = machine.intrinsics.builtins.symbol_iterator();
        let iterator_key = machine.to_property_key(iterator_symbol).unwrap();
        machine
            .set_data_property_key(iterable, iterator_key, create_fn)
            .unwrap();
        iterable
    }

    /// Creates an object-shaped entry with properties "0" and "1" (not an array).
    fn entry_pair(machine: &mut Machine<'_, TestHost>, key: Value, value: Value) -> Value {
        let entry = object(machine);
        machine.set_data_property(entry, "0", key).unwrap();
        machine.set_data_property(entry, "1", value).unwrap();
        entry
    }

    fn collection_entries(machine: &Machine<'_, TestHost>, obj: Value) -> Vec<(Value, Value)> {
        let slot = collection_slot(machine, obj).unwrap();
        let HeapEntry::Collection { entries, .. } = &machine.heap[slot] else {
            panic!("not a collection")
        };
        entries.iter().map(|e| (e.key, e.value)).collect()
    }

    // ---- tests -------------------------------------------------------------

    #[test]
    fn map_consumes_generic_iterable() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let e1 = allocate_array(&mut machine, vec![Value::int32(1), Value::int32(10)]).unwrap();
        let e2 = allocate_array(&mut machine, vec![Value::int32(2), Value::int32(20)]).unwrap();
        let source = custom_iterable(&mut machine, vec![e1, e2]);

        let map = construct_builtin(&mut machine, "Map", &[source]);
        let entries = collection_entries(&machine, map);
        assert_eq!(
            entries,
            vec![
                (Value::int32(1), Value::int32(10)),
                (Value::int32(2), Value::int32(20)),
            ]
        );
    }

    #[test]
    fn map_accepts_object_shaped_entries() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let e1 = entry_pair(&mut machine, Value::int32(1), Value::int32(10));
        let e2 = entry_pair(&mut machine, Value::int32(2), Value::int32(20));
        let source = custom_iterable(&mut machine, vec![e1, e2]);

        let map = construct_builtin(&mut machine, "Map", &[source]);
        let entries = collection_entries(&machine, map);
        assert_eq!(
            entries,
            vec![
                (Value::int32(1), Value::int32(10)),
                (Value::int32(2), Value::int32(20)),
            ]
        );
    }

    #[test]
    fn map_rejects_primitive_entries() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Iterator yields a primitive number, not an object-shaped entry.
        let source = custom_iterable(&mut machine, vec![Value::int32(42)]);
        let id = builtin_id(&machine, "Map");
        let result = machine.call_builtin(id, Value::UNDEFINED, &[source], true);
        assert!(result.is_err(), "Map with primitive entry must fail");
    }

    #[test]
    fn set_consumes_generic_iterable() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = custom_iterable(
            &mut machine,
            vec![Value::int32(10), Value::int32(20), Value::int32(30)],
        );

        let set = construct_builtin(&mut machine, "Set", &[source]);
        let entries = collection_entries(&machine, set);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].1, Value::int32(10));
        assert_eq!(entries[1].1, Value::int32(20));
        assert_eq!(entries[2].1, Value::int32(30));
    }

    #[test]
    fn weak_map_preserves_weak_key_check_from_iterable() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Entry with a primitive key — WeakMap must reject it even though the
        // entry comes through the iterator protocol.
        let e1 = entry_pair(&mut machine, Value::int32(1), Value::int32(10));
        let source = custom_iterable(&mut machine, vec![e1]);

        let id = builtin_id(&machine, "WeakMap");
        let result = machine.call_builtin(id, Value::UNDEFINED, &[source], true);
        assert!(
            result.is_err(),
            "WeakMap must reject primitive key from iterable"
        );
    }
    #[test]
    fn map_consumes_array_through_protocol() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let e1 = allocate_array(&mut machine, vec![Value::int32(1), Value::int32(10)]).unwrap();
        let e2 = allocate_array(&mut machine, vec![Value::int32(2), Value::int32(20)]).unwrap();
        let source = allocate_array(&mut machine, vec![e1, e2]).unwrap();

        let map = construct_builtin(&mut machine, "Map", &[source]);
        let entries = collection_entries(&machine, map);
        assert_eq!(
            entries,
            vec![
                (Value::int32(1), Value::int32(10)),
                (Value::int32(2), Value::int32(20)),
            ]
        );
    }

    // ---- %GeneratorPrototype% tests ----------------------------------------

    #[test]
    fn generator_prototype_chains_to_iterator_prototype() {
        let module = module();
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());

        let gen_proto = machine.intrinsics.builtins.generator_prototype();
        let iter_proto = machine.intrinsics.builtins.iterator_prototype();
        assert!(
            machine.inherits_from_prototype(gen_proto, iter_proto).unwrap(),
            "%GeneratorPrototype% must inherit from %IteratorPrototype%"
        );
    }

    #[test]
    fn generator_prototype_next_has_length_one() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let gen_proto = machine.intrinsics.builtins.generator_prototype();
        let next = machine.get_named_property(gen_proto, "next").unwrap();
        let index = machine.runtime_slot(next).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("next must be a native function");
        };
        let def = machine.intrinsics.builtins.get(id);
        assert_eq!(def.length, 1, "Generator.prototype.next length must be 1");
        assert_eq!(def.name, "next");
    }

    #[test]
    fn generator_prototype_inherits_symbol_iterator_identity() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let gen_proto = machine.intrinsics.builtins.generator_prototype();
        let iter_proto = machine.intrinsics.builtins.iterator_prototype();
        let symbol = machine.intrinsics.builtins.symbol_iterator();
        let key = machine.to_property_key(symbol).unwrap();

        // %GeneratorPrototype% has no own Symbol.iterator — it inherits from
        // %IteratorPrototype%, and the inherited function returns the same
        // identity (the receiver itself).
        let gen_identity = machine.get_property_key(gen_proto, &key).unwrap();
        let iter_identity = machine.get_property_key(iter_proto, &key).unwrap();
        assert_eq!(
            gen_identity, iter_identity,
            "Symbol.iterator on %GeneratorPrototype% must be the same \
             function inherited from %IteratorPrototype%"
        );
    }

    #[test]
    fn generator_next_on_incompatible_receiver_typeerrors() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // An ordinary object is not a generator — take_generator_state (the
        // centralized driver validation) must reject it with a TypeError.
        let non_generator = object(&mut machine);
        let result = machine.take_generator_state(non_generator);
        assert!(
            matches!(result, Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))),
            "next on a non-generator must produce a TypeError"
        );
    }

    #[test]
    fn generator_next_outcome_packages_this_and_resume_value() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let gen_proto = machine.intrinsics.builtins.generator_prototype();
        let next = machine.get_named_property(gen_proto, "next").unwrap();
        let index = machine.runtime_slot(next).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("next must be a native function");
        };

        let receiver = object(&mut machine);
        let resume = Value::int32(42);
        let outcome = machine
            .call_builtin(id, receiver, &[resume], false)
            .unwrap();
        match outcome {
            BuiltinOutcome::GeneratorNext {
                generator,
                resume_value,
            } => {
                assert_eq!(generator, receiver);
                assert_eq!(resume_value, resume);
            }
            other => panic!("expected GeneratorNext, got {other:?}"),
        }

        // Without arguments, resume_value defaults to undefined.
        let outcome = machine.call_builtin(id, receiver, &[], false).unwrap();
        match outcome {
            BuiltinOutcome::GeneratorNext { resume_value, .. } => {
                assert_eq!(resume_value, Value::UNDEFINED);
            }
            other => panic!("expected GeneratorNext, got {other:?}"),
        }
    }

    #[test]
    fn iterator_result_completed_shape() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let value = Value::int32(99);
        let result = machine.iterator_result(value, true).unwrap();
        let result_value = machine.get_named_property(result, "value").unwrap();
        let done = machine.get_named_property(result, "done").unwrap();
        assert_eq!(result_value, value);
        assert_eq!(done, Value::TRUE);
    }
}
