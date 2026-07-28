use std::collections::BTreeMap;

use bamts_native::{Decoded, Value};

use super::{allocate_array, allocate_string, define_data, install_function, type_error};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.object_prototype();
    let constructor = install_function(heap, builtins, "Object", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert("Object".to_owned(), constructor);

    for (name, length, handler) in [
        ("keys", 1, keys::<H> as BuiltinHandler<H>),
        ("values", 1, values::<H>),
        ("entries", 1, entries::<H>),
        ("assign", 2, assign::<H>),
        ("create", 2, create::<H>),
        ("freeze", 1, freeze::<H>),
        ("isFrozen", 1, is_frozen::<H>),
        ("defineProperty", 3, define_property::<H>),
        ("defineProperties", 2, define_properties::<H>),
        ("getOwnPropertyNames", 1, get_own_property_names::<H>),
        (
            "getOwnPropertyDescriptor",
            2,
            get_own_property_descriptor::<H>,
        ),
        ("getPrototypeOf", 1, get_prototype_of::<H>),
        ("setPrototypeOf", 2, set_prototype_of::<H>),
        ("fromEntries", 1, from_entries::<H>),
        ("hasOwn", 2, has_own::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        machine_static(heap, constructor, name, function);
    }

    for (name, length, handler) in [
        ("toString", 0, prototype_to_string::<H> as BuiltinHandler<H>),
        ("hasOwnProperty", 1, has_own_property::<H>),
        ("isPrototypeOf", 1, is_prototype_of::<H>),
        ("valueOf", 0, value_of::<H>),
        ("propertyIsEnumerable", 1, property_is_enumerable::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
        if name == "toString" {
            globals.insert("\0Object.prototype.toString".to_owned(), function);
        }
    }
    let call = install_function(heap, builtins, "call", 1, function_call::<H>);
    define_data(heap, builtins.function_prototype(), "call", call);
    globals.insert("\0Function.prototype.call".to_owned(), call);
}

fn machine_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let index = super::heap_index(constructor);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[index] else {
        panic!("constructor must be native");
    };
    properties.insert(
        PropertyKey::Named(name.to_owned()),
        super::builtin_property(value),
    );
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let first = args.first().copied().unwrap_or(Value::UNDEFINED);
    if machine.is_object(first) {
        return Ok(BuiltinOutcome::Value(first));
    }
    let value = match first.decode() {
        Some(Decoded::Undefined | Decoded::Null) => machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?,
        _ => machine.box_primitive(first)?,
    };
    Ok(BuiltinOutcome::Value(value))
}

fn object_arg<H: Host>(
    _machine: &Machine<'_, H>,
    args: &[Value],
    operation: &'static str,
) -> Result<Value, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    if matches!(value.decode(), Some(Decoded::Undefined | Decoded::Null)) {
        return Err(type_error(operation));
    }
    Ok(value)
}

fn own_names<H: Host>(machine: &Machine<'_, H>, value: Value) -> Result<Vec<String>, EvalFailure> {
    Ok(machine
        .own_property_keys(value)?
        .into_iter()
        .filter_map(|key| match key {
            PropertyKey::Named(name) => Some(name),
            PropertyKey::Private(_) => None,
        })
        .collect())
}

fn keys<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let values = machine
        .enumerable_keys(value)?
        .into_iter()
        .map(|name| allocate_string(machine, name))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
}

fn values<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let mut output = Vec::new();
    for name in machine.enumerable_keys(source)? {
        output.push(machine.get_named_property(source, &name)?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, output)?))
}

fn entries<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let mut output = Vec::new();
    for name in machine.enumerable_keys(source)? {
        let key = allocate_string(machine, name.clone())?;
        let value = machine.get_named_property(source, &name)?;
        output.push(allocate_array(machine, vec![key, value])?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, output)?))
}

fn assign<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    for source in args.iter().copied().skip(1) {
        if matches!(source.decode(), Some(Decoded::Undefined | Decoded::Null)) {
            continue;
        }
        for name in machine.enumerable_keys(source)? {
            let value = machine.get_named_property(source, &name)?;
            machine.set_data_property(target, &name, value)?;
        }
    }
    Ok(BuiltinOutcome::Value(target))
}

fn create<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let prototype = args.first().copied().unwrap_or(Value::UNDEFINED);
    if prototype != Value::NULL && !machine.is_object(prototype) {
        return Err(type_error("Object prototype may only be an Object or null"));
    }
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: (prototype != Value::NULL).then_some(prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    if let Some(descriptors) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        define_properties_on(machine, object, descriptors)?;
    }
    Ok(BuiltinOutcome::Value(object))
}

fn freeze<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    machine.mark_frozen(value)?;
    Ok(BuiltinOutcome::Value(value))
}

fn is_frozen<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine.is_frozen_value(value)?,
    )))
}

fn descriptor_from<H: Host>(
    machine: &mut Machine<'_, H>,
    descriptor: Value,
) -> Result<Property, EvalFailure> {
    if !machine.is_object(descriptor) {
        return Err(type_error("Property description must be an object"));
    }
    let enumerable_value = machine.get_named_property(descriptor, "enumerable")?;
    let enumerable = machine.to_boolean(enumerable_value);
    let configurable_value = machine.get_named_property(descriptor, "configurable")?;
    let configurable = machine.to_boolean(configurable_value);
    let getter = machine.get_named_property(descriptor, "get")?;
    let setter = machine.get_named_property(descriptor, "set")?;
    let accessor = getter != Value::UNDEFINED || setter != Value::UNDEFINED;
    if accessor {
        if (getter != Value::UNDEFINED && !machine.is_callable(getter)?)
            || (setter != Value::UNDEFINED && !machine.is_callable(setter)?)
            || machine.has_own_named_property(descriptor, "value")?
            || machine.has_own_named_property(descriptor, "writable")?
        {
            return Err(type_error("Invalid property descriptor"));
        }
        return Ok(Property::Accessor {
            getter: (getter != Value::UNDEFINED).then_some(getter),
            setter: (setter != Value::UNDEFINED).then_some(setter),
            enumerable,
            configurable,
        });
    }
    let value = machine.get_named_property(descriptor, "value")?;
    let writable_value = machine.get_named_property(descriptor, "writable")?;
    Ok(Property::Data {
        value,
        writable: machine.to_boolean(writable_value),
        enumerable,
        configurable,
    })
}

fn define_property<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Object.defineProperty called on non-object")?;
    if !machine.is_object(target) {
        return Err(type_error("Object.defineProperty called on non-object"));
    }
    let key = machine.to_string(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    let descriptor = descriptor_from(machine, args.get(2).copied().unwrap_or(Value::UNDEFINED))?;
    machine.define_named_descriptor(target, key, descriptor)?;
    Ok(BuiltinOutcome::Value(target))
}

fn define_properties_on<H: Host>(
    machine: &mut Machine<'_, H>,
    target: Value,
    descriptors: Value,
) -> Result<(), EvalFailure> {
    for key in machine.enumerable_keys(descriptors)? {
        let descriptor = machine.get_named_property(descriptors, &key)?;
        let descriptor = descriptor_from(machine, descriptor)?;
        machine.define_named_descriptor(target, key, descriptor)?;
    }
    Ok(())
}

fn define_properties<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(
        machine,
        args,
        "Object.defineProperties called on non-object",
    )?;
    if !machine.is_object(target) {
        return Err(type_error("Object.defineProperties called on non-object"));
    }
    let descriptors = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    define_properties_on(machine, target, descriptors)?;
    Ok(BuiltinOutcome::Value(target))
}

fn get_own_property_names<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let names = own_names(machine, value)?
        .into_iter()
        .map(|name| allocate_string(machine, name))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BuiltinOutcome::Value(allocate_array(machine, names)?))
}

fn get_own_property_descriptor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let key = machine.to_string(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    let Some(property) = machine.own_named_descriptor(target, &key)? else {
        return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
    };
    let descriptor = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.object_prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    match property {
        Property::Data {
            value,
            writable,
            enumerable,
            configurable,
        } => {
            machine.set_data_property(descriptor, "value", value)?;
            machine.set_data_property(descriptor, "writable", Value::boolean(writable))?;
            machine.set_data_property(descriptor, "enumerable", Value::boolean(enumerable))?;
            machine.set_data_property(descriptor, "configurable", Value::boolean(configurable))?;
        }
        Property::Accessor {
            getter,
            setter,
            enumerable,
            configurable,
        } => {
            machine.set_data_property(descriptor, "get", getter.unwrap_or(Value::UNDEFINED))?;
            machine.set_data_property(descriptor, "set", setter.unwrap_or(Value::UNDEFINED))?;
            machine.set_data_property(descriptor, "enumerable", Value::boolean(enumerable))?;
            machine.set_data_property(descriptor, "configurable", Value::boolean(configurable))?;
        }
    }
    Ok(BuiltinOutcome::Value(descriptor))
}

fn get_prototype_of<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    Ok(BuiltinOutcome::Value(
        machine.prototype_value(target)?.unwrap_or(Value::NULL),
    ))
}

fn set_prototype_of<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(
        machine,
        args,
        "Object.setPrototypeOf called on null or undefined",
    )?;
    let prototype = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    if prototype != Value::NULL && !machine.is_object(prototype) {
        return Err(type_error("Object prototype may only be an Object or null"));
    }
    machine.set_prototype_value(target, (prototype != Value::NULL).then_some(prototype))?;
    Ok(BuiltinOutcome::Value(target))
}

fn from_entries<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let iterable = args.first().copied().unwrap_or(Value::UNDEFINED);
    let entries = machine
        .array_elements(iterable)?
        .ok_or_else(|| type_error("Object.fromEntries requires an iterable"))?;
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.object_prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    for entry in entries.into_iter().filter(|value| *value != Value::HOLE) {
        let pair = machine
            .array_elements(entry)?
            .ok_or_else(|| type_error("Iterator value is not an entry object"))?;
        let key = machine.to_string(pair.first().copied().unwrap_or(Value::UNDEFINED))?;
        let value = pair.get(1).copied().unwrap_or(Value::UNDEFINED);
        machine.set_data_property(object, &key, value)?;
    }
    Ok(BuiltinOutcome::Value(object))
}

fn has_own<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let key = machine.to_string(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine.has_own_named_property(target, &key)?,
    )))
}

fn prototype_to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let tag = machine.object_to_string_tag(this)?;
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        format!("[object {tag}]"),
    )?))
}

fn has_own_property<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine.has_own_named_property(this, &key)?,
    )))
}

fn is_prototype_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut value = args.first().copied().unwrap_or(Value::UNDEFINED);
    while let Some(prototype) = machine.prototype_value(value)? {
        if prototype == this {
            return Ok(BuiltinOutcome::Value(Value::TRUE));
        }
        value = prototype;
    }
    Ok(BuiltinOutcome::Value(Value::FALSE))
}

fn value_of<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(this))
}

fn property_is_enumerable<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let enumerable = machine
        .own_named_descriptor(this, &key)?
        .is_some_and(|property| property.enumerable());
    Ok(BuiltinOutcome::Value(Value::boolean(enumerable)))
}

fn function_call<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Call {
        callee: this,
        this_value: args.first().copied().unwrap_or(Value::UNDEFINED),
        argument_start: usize::from(!args.is_empty()),
    })
}

pub(super) fn structured_clone<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    let mut seen = BTreeMap::new();
    Ok(BuiltinOutcome::Value(clone_value(
        machine, value, &mut seen,
    )?))
}

fn clone_value<H: Host>(
    machine: &mut Machine<'_, H>,
    value: Value,
    seen: &mut BTreeMap<usize, Value>,
) -> Result<Value, EvalFailure> {
    let Some(Decoded::HeapRef(_)) = value.decode() else {
        return Ok(value);
    };
    let index = machine
        .runtime_slot(value)
        .map_err(EvalFailure::Runtime)?
        .ok_or_else(|| type_error("cannot clone host object"))?;
    if let Some(clone) = seen.get(&index) {
        return Ok(*clone);
    }
    match machine.heap[index].clone() {
        HeapEntry::String(text) => allocate_string(machine, text),
        HeapEntry::Array { elements, .. } => {
            let clone = allocate_array(machine, vec![Value::HOLE; elements.len()])?;
            seen.insert(index, clone);
            let mut copied = vec![Value::HOLE; elements.len()];
            for (offset, element) in elements.into_iter().enumerate() {
                if element != Value::HOLE {
                    copied[offset] = clone_value(machine, element, seen)?;
                }
            }
            machine.replace_array_elements(clone, copied)?;
            Ok(clone)
        }
        HeapEntry::Object {
            properties,
            prototype,
            ..
        } => {
            let clone = machine
                .allocate(HeapEntry::Object {
                    properties: PropertyMap::default(),
                    prototype,
                    extensible: true,
                    boxed_primitive: None,
                })
                .map_err(EvalFailure::Runtime)?;
            seen.insert(index, clone);
            for (key, _) in properties.0 {
                if let PropertyKey::Named(name) = key {
                    let source = machine.get_named_property(value, &name)?;
                    let copied = clone_value(machine, source, seen)?;
                    machine.set_data_property(clone, &name, copied)?;
                }
            }
            Ok(clone)
        }
        _ => Err(type_error("value could not be cloned")),
    }
}
