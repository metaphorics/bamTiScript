use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_string, builtin_property, define_data, heap_index, install_function, type_error,
};
use crate::intrinsics::{BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let iterator = symbol(heap, "Symbol.iterator");
    let async_iterator = symbol(heap, "Symbol.asyncIterator");
    let has_instance = symbol(heap, "Symbol.hasInstance");
    let to_string_tag = symbol(heap, "Symbol.toStringTag");
    builtins.set_symbol_iterator(iterator);
    builtins.set_symbol_to_string_tag(to_string_tag);
    builtins.set_symbol_prototype(prototype);

    let constructor = install_function(heap, builtins, "Symbol", 0, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    let symbol_for = install_function(heap, builtins, "for", 1, symbol_for::<H>);
    define_native_property(heap, constructor, "for", symbol_for);
    for (name, value) in [
        ("iterator", iterator),
        ("asyncIterator", async_iterator),
        ("hasInstance", has_instance),
        ("toStringTag", to_string_tag),
    ] {
        define_readonly_property(heap, constructor, name, value);
    }

    let to_string = install_function(heap, builtins, "toString", 0, to_string::<H>);
    let value_of = install_function(heap, builtins, "valueOf", 0, value_of::<H>);
    let description = install_function(heap, builtins, "get description", 0, description::<H>);
    define_data(heap, prototype, "toString", to_string);
    define_data(heap, prototype, "valueOf", value_of);
    let symbol_tag = allocate_literal_string(heap, "Symbol");
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8("description")),
        Property::Accessor {
            getter: Some(description),
            setter: None,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        PropertyKey::Symbol(heap_index(to_string_tag) as u32),
        builtin_property(symbol_tag),
    );

    globals.insert(EcmaString::from_utf8("Symbol"), constructor);
}

fn symbol(heap: &mut Vec<HeapEntry>, description: &str) -> Value {
    super::super::push(
        heap,
        HeapEntry::Symbol {
            description: EcmaString::from_utf8(description),
        },
    )
}

fn allocate_literal_string(heap: &mut Vec<HeapEntry>, text: &str) -> Value {
    super::super::push(heap, HeapEntry::String(EcmaString::from_utf8(text)))
}

fn define_native_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        builtin_property(value),
    );
}

fn define_readonly_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        Property::Data {
            value,
            writable: false,
            enumerable: false,
            configurable: false,
        },
    );
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error("Symbol is not a constructor"));
    }
    let description = args
        .first()
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
        .map(|value| machine.to_string(value))
        .transpose()?
        .unwrap_or_default();
    let symbol = machine
        .allocate(HeapEntry::Symbol { description })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(symbol))
}

fn symbol_for<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if let Some(existing) = machine.intrinsics.symbol_registry.get(&key).copied() {
        return Ok(BuiltinOutcome::Value(existing));
    }
    let symbol = machine
        .allocate(HeapEntry::Symbol {
            description: key.clone(),
        })
        .map_err(EvalFailure::Runtime)?;
    machine
        .charge_heap(PropertyKey::Named(key.clone()).charge_bytes())
        .map_err(EvalFailure::Runtime)?;
    machine.intrinsics.symbol_registry.insert(key, symbol);
    Ok(BuiltinOutcome::Value(symbol))
}

fn symbol_description<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Result<EcmaString, EvalFailure> {
    let Some(Decoded::HeapRef(id)) = value.decode() else {
        return Err(type_error("Symbol method called on incompatible receiver"));
    };
    let index = id.slot() as usize - 1;
    match machine.heap.get(index) {
        Some(HeapEntry::Symbol { description }) => Ok(description.clone()),
        _ => Err(type_error("Symbol method called on incompatible receiver")),
    }
}

fn description<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let description = symbol_description(machine, this)?;
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        description,
    )?))
}

fn to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let description = symbol_description(machine, this)?;
    let mut builder =
        bamts_bytecode::EcmaStringBuilder::with_capacity(description.len_units().saturating_add(8));
    builder.push_utf8("Symbol(");
    for &unit in description.as_units() {
        builder.push_unit(unit);
    }
    builder.push_unit(u16::from(b')'));
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        builder.finish(),
    )?))
}

fn value_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    symbol_description(machine, this)?;
    Ok(BuiltinOutcome::Value(this))
}
