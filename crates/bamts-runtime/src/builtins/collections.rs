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
        let entries = machine
            .array_elements(source)?
            .ok_or_else(|| type_error("collection constructor argument is not iterable"))?;
        for entry in entries {
            let pair = machine
                .array_elements(entry)?
                .ok_or_else(|| type_error("Iterator value is not an entry object"))?;
            let key = pair.first().copied().unwrap_or(Value::UNDEFINED);
            if name == "WeakMap" {
                require_weak_key(machine, key)?;
            }
            map_put(
                machine,
                object,
                key,
                pair.get(1).copied().unwrap_or(Value::UNDEFINED),
            )?;
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
        let values = machine
            .array_elements(source)?
            .ok_or_else(|| type_error("collection constructor argument is not iterable"))?;
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
