use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, range_error, type_error,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.object_prototype();
    let constructor = install_function(heap, builtins, "Object", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::from_utf8("Object"), constructor);

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
        ("getOwnPropertySymbols", 1, get_own_property_symbols::<H>),
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
            builtins.set_object_to_string(function);
        }
    }
    let call = install_function(heap, builtins, "call", 1, function_call::<H>);
    define_data(heap, builtins.function_prototype(), "call", call);
    builtins.set_function_call(call);
}

fn machine_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let index = super::heap_index(constructor);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[index] else {
        panic!("constructor must be native");
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
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

fn own_names<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Result<Vec<EcmaString>, EvalFailure> {
    Ok(machine
        .own_property_keys(value)?
        .into_iter()
        .filter_map(|key| match key {
            PropertyKey::Named(name) => Some(name),
            PropertyKey::Symbol(_) | PropertyKey::Private(_) => None,
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
        output.push(machine.get_property_key(source, &PropertyKey::Named(name))?);
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
        let value = machine.get_property_key(source, &PropertyKey::Named(name))?;
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
            let key = PropertyKey::Named(name);
            let value = machine.get_property_key(source, &key)?;
            machine.set_data_property_key(target, key, value)?;
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

#[derive(Clone, Copy, Debug)]
struct PropertyDescriptor {
    value: Option<Value>,
    writable: Option<bool>,
    getter: Option<Value>,
    setter: Option<Value>,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

impl PropertyDescriptor {
    fn is_accessor(self) -> bool {
        self.getter.is_some() || self.setter.is_some()
    }

    fn is_data(self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    fn into_property(self, current: Option<Property>) -> Property {
        let enumerable = self
            .enumerable
            .unwrap_or_else(|| current.as_ref().is_some_and(Property::enumerable));
        let configurable = self
            .configurable
            .unwrap_or_else(|| current.as_ref().is_some_and(Property::configurable));

        if self.is_accessor() {
            let (current_getter, current_setter) = match current {
                Some(Property::Accessor { getter, setter, .. }) => (getter, setter),
                _ => (None, None),
            };
            return Property::Accessor {
                getter: self
                    .getter
                    .map(|value| (value != Value::UNDEFINED).then_some(value))
                    .unwrap_or(current_getter),
                setter: self
                    .setter
                    .map(|value| (value != Value::UNDEFINED).then_some(value))
                    .unwrap_or(current_setter),
                enumerable,
                configurable,
            };
        }

        if self.is_data() {
            let (current_value, current_writable) = match current {
                Some(Property::Data {
                    value, writable, ..
                }) => (value, writable),
                _ => (Value::UNDEFINED, false),
            };
            return Property::Data {
                value: self.value.unwrap_or(current_value),
                writable: self.writable.unwrap_or(current_writable),
                enumerable,
                configurable,
            };
        }

        match current {
            Some(Property::Accessor { getter, setter, .. }) => Property::Accessor {
                getter,
                setter,
                enumerable,
                configurable,
            },
            Some(Property::Data {
                value, writable, ..
            }) => Property::Data {
                value,
                writable,
                enumerable,
                configurable,
            },
            None => Property::Data {
                value: Value::UNDEFINED,
                writable: false,
                enumerable,
                configurable,
            },
        }
    }
}

fn descriptor_field<H: Host>(
    machine: &mut Machine<'_, H>,
    descriptor: Value,
    name: &str,
) -> Result<Option<Value>, EvalFailure> {
    let key = PropertyKey::Named(EcmaString::from_utf8(name));
    if !machine.has_property(descriptor, &key)? {
        return Ok(None);
    }
    machine.get_property_key(descriptor, &key).map(Some)
}

fn descriptor_from<H: Host>(
    machine: &mut Machine<'_, H>,
    descriptor: Value,
) -> Result<PropertyDescriptor, EvalFailure> {
    if !machine.is_object(descriptor) {
        return Err(type_error("Property description must be an object"));
    }
    let enumerable =
        descriptor_field(machine, descriptor, "enumerable")?.map(|value| machine.to_boolean(value));
    let configurable = descriptor_field(machine, descriptor, "configurable")?
        .map(|value| machine.to_boolean(value));
    let value = descriptor_field(machine, descriptor, "value")?;
    let writable =
        descriptor_field(machine, descriptor, "writable")?.map(|value| machine.to_boolean(value));
    let getter = descriptor_field(machine, descriptor, "get")?;
    if let Some(getter) = getter
        && getter != Value::UNDEFINED
        && !machine.is_callable(getter)?
    {
        return Err(type_error("Invalid property descriptor"));
    }
    let setter = descriptor_field(machine, descriptor, "set")?;
    if let Some(setter) = setter
        && setter != Value::UNDEFINED
        && !machine.is_callable(setter)?
    {
        return Err(type_error("Invalid property descriptor"));
    }
    if (getter.is_some() || setter.is_some()) && (value.is_some() || writable.is_some()) {
        return Err(type_error("Invalid property descriptor"));
    }
    Ok(PropertyDescriptor {
        value,
        writable,
        getter,
        setter,
        enumerable,
        configurable,
    })
}

fn define_array_length_descriptor<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    key: &PropertyKey,
    descriptor: PropertyDescriptor,
) -> Result<bool, EvalFailure> {
    let Some(index) = machine.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
        return Ok(false);
    };
    if !matches!(key, PropertyKey::Named(name) if name.eq_ascii("length"))
        || !matches!(machine.heap[index], HeapEntry::Array { .. })
    {
        return Ok(false);
    }
    if descriptor.is_accessor() {
        return Err(type_error("Invalid property descriptor"));
    }
    if descriptor.enumerable == Some(true) || descriptor.configurable == Some(true) {
        return Err(type_error("Cannot redefine array length"));
    }
    let length = descriptor
        .value
        .map(|value| {
            crate::exact_array_length(value).ok_or_else(|| range_error("define array length"))
        })
        .transpose()?;
    let HeapEntry::Array {
        elements,
        properties,
        length_writable,
        ..
    } = &mut machine.heap[index]
    else {
        unreachable!("array checked above");
    };
    if descriptor.writable == Some(true) && !*length_writable {
        return Err(type_error("Cannot make array length writable"));
    }
    let result = match length {
        Some(length) => super::define_array_length(elements, properties, *length_writable, length),
        None => Ok(()),
    };
    if descriptor.writable == Some(false) {
        *length_writable = false;
    }
    result?;
    Ok(true)
}

fn apply_property_descriptor<H: Host>(
    machine: &mut Machine<'_, H>,
    target: Value,
    key: PropertyKey,
    descriptor: PropertyDescriptor,
) -> Result<(), EvalFailure> {
    if define_array_length_descriptor(machine, target, &key, descriptor)? {
        return Ok(());
    }
    let current = machine.own_descriptor(target, &key)?;
    machine.define_descriptor(target, key, descriptor.into_property(current))
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
    let key = machine.to_property_key(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    let descriptor = descriptor_from(machine, args.get(2).copied().unwrap_or(Value::UNDEFINED))?;
    apply_property_descriptor(machine, target, key, descriptor)?;
    Ok(BuiltinOutcome::Value(target))
}

fn define_properties_on<H: Host>(
    machine: &mut Machine<'_, H>,
    target: Value,
    descriptors: Value,
) -> Result<(), EvalFailure> {
    let mut definitions = Vec::new();
    for key in machine.own_property_keys(descriptors)? {
        if !machine
            .own_descriptor(descriptors, &key)?
            .is_some_and(|property| property.enumerable())
        {
            continue;
        }
        let descriptor = machine.get_property_key(descriptors, &key)?;
        definitions.push((key, descriptor_from(machine, descriptor)?));
    }
    for (key, descriptor) in definitions {
        apply_property_descriptor(machine, target, key, descriptor)?;
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

fn get_own_property_symbols<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let symbols = machine
        .own_property_keys(value)?
        .into_iter()
        .filter_map(|key| match key {
            PropertyKey::Symbol(index) => Some(Value::heap_ref(
                bamts_native::SlotId::from_parts(crate::RUNTIME_HEAP_SEGMENT, index + 1)
                    .expect("property key is a valid runtime heap slot"),
            )),
            PropertyKey::Named(_) | PropertyKey::Private(_) => None,
        })
        .collect();
    Ok(BuiltinOutcome::Value(allocate_array(machine, symbols)?))
}

fn get_own_property_descriptor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let target = object_arg(machine, args, "Cannot convert undefined or null to object")?;
    let key = machine.to_property_key(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    let Some(property) = machine.own_descriptor(target, &key)? else {
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
        let key = machine.to_property_key(pair.first().copied().unwrap_or(Value::UNDEFINED))?;
        let value = pair.get(1).copied().unwrap_or(Value::UNDEFINED);
        machine.set_data_property_key(object, key, value)?;
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
    let key = machine.to_property_key(args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine.has_own_property_key(target, &key)?,
    )))
}

fn prototype_to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let tag = match this.decode() {
        Some(Decoded::Undefined) => EcmaString::from_utf8("Undefined"),
        Some(Decoded::Null) => EcmaString::from_utf8("Null"),
        _ => {
            let fallback = machine.object_to_string_tag(this)?;
            let symbol = machine.intrinsics.builtins.symbol_to_string_tag();
            let key = PropertyKey::Symbol(
                machine
                    .runtime_slot(symbol)
                    .map_err(EvalFailure::Runtime)?
                    .expect("well-known symbol belongs to the runtime heap") as u32,
            );
            let tag_value = machine.get_property_key(this, &key)?;
            machine
                .string_value(tag_value)
                .unwrap_or_else(|| EcmaString::from_utf8(fallback))
        }
    };
    let mut output =
        bamts_bytecode::EcmaStringBuilder::with_capacity(tag.len_units().saturating_add(9));
    output.push_utf8("[object ");
    for &unit in tag.as_units() {
        output.push_unit(unit);
    }
    output.push_unit(u16::from(b']'));
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}

fn has_own_property<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = machine.to_property_key(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine.has_own_property_key(this, &key)?,
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
    let key = machine.to_property_key(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let enumerable = machine
        .own_descriptor(this, &key)?
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
                    let key = PropertyKey::Named(name);
                    let source = machine.get_property_key(value, &key)?;
                    let copied = clone_value(machine, source, seen)?;
                    machine.set_data_property_key(clone, key, copied)?;
                }
            }
            Ok(clone)
        }
        _ => Err(type_error("value could not be cloned")),
    }
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Verified,
    };

    use super::*;
    use crate::Limits;

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

    fn data_descriptor(machine: &mut Machine<'_, TestHost>, value: Value) -> Value {
        let descriptor = object(machine);
        machine
            .set_data_property(descriptor, "value", value)
            .unwrap();
        machine
            .set_data_property(descriptor, "enumerable", Value::TRUE)
            .unwrap();
        descriptor
    }

    fn call_define_properties(
        machine: &mut Machine<'_, TestHost>,
        target: Value,
        descriptors: Value,
    ) -> Result<Value, EvalFailure> {
        let constructor = machine.intrinsics.global("Object").unwrap();
        let method = machine.get_named_property(constructor, "defineProperties")?;
        machine.call_value(method, constructor, &[target, descriptors])
    }

    fn call_object(
        machine: &mut Machine<'_, TestHost>,
        method_name: &str,
        args: &[Value],
    ) -> Result<Value, EvalFailure> {
        let constructor = machine.intrinsics.global("Object").unwrap();
        let method = machine.get_named_property(constructor, method_name)?;
        machine.call_value(method, constructor, args)
    }

    fn assert_unchanged(machine: &mut Machine<'_, TestHost>, target: Value) {
        assert_eq!(
            machine.get_named_property(target, "stable").unwrap(),
            Value::int32(9)
        );
        assert!(
            !machine
                .has_own_property_key(target, &PropertyKey::Named(EcmaString::from_utf8("first")))
                .unwrap()
        );
        assert!(
            !machine
                .has_own_property_key(target, &PropertyKey::Named(EcmaString::from_utf8("second")))
                .unwrap()
        );
    }

    fn symbol_key(machine: &Machine<'_, TestHost>, symbol: Value) -> PropertyKey {
        machine.to_property_key(symbol).unwrap()
    }

    fn symbol(machine: &mut Machine<'_, TestHost>, description: &str) -> Value {
        machine
            .allocate(HeapEntry::Symbol {
                description: EcmaString::from_utf8(description),
            })
            .unwrap()
    }

    #[test]
    fn object_reflection_preserves_symbol_keys_and_filters_string_names() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        let key = symbol(&mut machine, "key");
        let descriptor = data_descriptor(&mut machine, Value::int32(42));

        call_object(&mut machine, "defineProperty", &[target, key, descriptor]).unwrap();

        let property_key = symbol_key(&machine, key);
        assert_eq!(
            machine.get_property_key(target, &property_key).unwrap(),
            Value::int32(42)
        );
        let child = object(&mut machine);
        machine.set_prototype_value(child, Some(target)).unwrap();
        assert!(machine.has_property(child, &property_key).unwrap());
        let names = call_object(&mut machine, "getOwnPropertyNames", &[target]).unwrap();
        assert!(machine.array_elements(names).unwrap().unwrap().is_empty());
        let symbols = call_object(&mut machine, "getOwnPropertySymbols", &[target]).unwrap();
        assert_eq!(machine.array_elements(symbols).unwrap().unwrap(), vec![key]);
        let description = machine.get_named_property(key, "description").unwrap();
        assert!(
            machine
                .string_value(description)
                .is_some_and(|text| text.eq_ascii("key"))
        );
        let to_string = machine.get_named_property(key, "toString").unwrap();
        let display = machine.call_value(to_string, key, &[]).unwrap();
        assert!(
            machine
                .string_value(display)
                .is_some_and(|text| text.eq_ascii("Symbol(key)"))
        );
    }

    #[test]
    fn define_properties_collects_enumerable_symbol_descriptors() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        let descriptors = object(&mut machine);
        let key = symbol(&mut machine, "definition");
        let descriptor = data_descriptor(&mut machine, Value::int32(7));
        let property_key = symbol_key(&machine, key);
        machine
            .set_data_property_key(descriptors, property_key, descriptor)
            .unwrap();

        call_define_properties(&mut machine, target, descriptors).unwrap();

        let property_key = symbol_key(&machine, key);
        assert_eq!(
            machine.get_property_key(target, &property_key).unwrap(),
            Value::int32(7)
        );
    }

    #[test]
    fn existing_namespaces_expose_standard_to_string_tags() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object_to_string = machine.intrinsics.object_to_string();

        for (name, expected) in [("Math", "[object Math]"), ("JSON", "[object JSON]")] {
            let namespace = machine.intrinsics.global(name).unwrap();
            let result = machine
                .call_value(object_to_string, namespace, &[])
                .unwrap();
            assert!(
                machine
                    .string_value(result)
                    .is_some_and(|text| text.eq_ascii(expected))
            );
        }
    }

    #[test]
    fn object_define_property_keeps_array_index_semantics() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(&mut machine, Vec::new()).unwrap();
        let descriptor = data_descriptor(&mut machine, Value::int32(9));

        call_object(
            &mut machine,
            "defineProperty",
            &[array, Value::int32(0), descriptor],
        )
        .unwrap();

        assert_eq!(
            machine.get_named_property(array, "0").unwrap(),
            Value::int32(9)
        );
        let length = machine.get_named_property(array, "length").unwrap();
        assert!(machine.to_string(length).unwrap().eq_ascii("1"));
    }

    #[test]
    fn object_to_string_uses_string_tags_and_evaluates_tag_accessors() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let tag_key = symbol_key(&machine, machine.intrinsics.builtins.symbol_to_string_tag());
        let object_to_string = machine.intrinsics.object_to_string();

        let non_string_tag = object(&mut machine);
        machine
            .set_data_property_key(non_string_tag, tag_key.clone(), Value::int32(1))
            .unwrap();
        let value = machine
            .call_value(object_to_string, non_string_tag, &[])
            .unwrap();
        assert!(
            machine
                .string_value(value)
                .is_some_and(|text| text.eq_ascii("[object Object]"))
        );

        let accessor_tag = object(&mut machine);
        let object_constructor = machine.intrinsics.global("Object").unwrap();
        let throwing_getter = machine
            .get_named_property(object_constructor, "defineProperty")
            .unwrap();
        machine
            .define_descriptor(
                accessor_tag,
                tag_key,
                Property::Accessor {
                    getter: Some(throwing_getter),
                    setter: None,
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        assert!(
            machine
                .call_value(object_to_string, accessor_tag, &[])
                .is_err()
        );
    }

    #[test]
    fn object_reflection_hides_language_private_keys() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        let symbol = symbol(&mut machine, "public");
        let private = machine
            .allocate(HeapEntry::PrivateName {
                description: EcmaString::from_utf8("private"),
            })
            .unwrap();
        machine
            .set_data_property_key(target, symbol_key(&machine, symbol), Value::int32(1))
            .unwrap();
        machine
            .set_data_property_key(
                target,
                machine.to_property_key(private).unwrap(),
                Value::int32(2),
            )
            .unwrap();

        let symbols = call_object(&mut machine, "getOwnPropertySymbols", &[target]).unwrap();
        assert_eq!(
            machine.array_elements(symbols).unwrap().unwrap(),
            vec![symbol]
        );
        let names = call_object(&mut machine, "getOwnPropertyNames", &[target]).unwrap();
        assert!(machine.array_elements(names).unwrap().unwrap().is_empty());
    }

    #[test]
    fn array_length_is_exotic_and_locks_index_growth() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(&mut machine, vec![Value::int32(1), Value::int32(2)]).unwrap();

        machine
            .set_data_property(array, "length", Value::int32(1))
            .unwrap();
        assert_eq!(
            machine.array_elements(array).unwrap().unwrap(),
            vec![Value::int32(1)]
        );
        machine
            .set_data_property(array, "length", Value::int32(3))
            .unwrap();
        assert_eq!(
            machine.array_elements(array).unwrap().unwrap(),
            vec![Value::int32(1), Value::HOLE, Value::HOLE]
        );
        let length = machine
            .own_descriptor(array, &PropertyKey::Named(EcmaString::from_utf8("length")))
            .unwrap()
            .unwrap();
        assert!(matches!(
            length,
            Property::Data {
                value,
                writable: true,
                enumerable: false,
                configurable: false,
            } if value == crate::number_value(3.0)
        ));
        assert!(
            machine
                .set_data_property(array, "length", crate::number_value(1.5))
                .is_err()
        );
        assert!(
            machine
                .set_data_property(array, "length", crate::number_value(u32::MAX as f64 + 1.0))
                .is_err()
        );

        let locked = object(&mut machine);
        machine
            .set_data_property(locked, "writable", Value::FALSE)
            .unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::from_utf8("length")).unwrap();
        call_object(&mut machine, "defineProperty", &[array, length_key, locked]).unwrap();
        let same_length = object(&mut machine);
        machine
            .set_data_property(same_length, "value", Value::int32(3))
            .unwrap();
        call_object(
            &mut machine,
            "defineProperty",
            &[array, length_key, same_length],
        )
        .unwrap();
        assert!(
            machine
                .set_data_property(array, "length", Value::int32(1))
                .is_err()
        );
        assert!(
            machine
                .set_data_property(array, "3", Value::int32(3))
                .is_err()
        );
        let index_descriptor = data_descriptor(&mut machine, Value::int32(3));
        assert!(
            call_object(
                &mut machine,
                "defineProperty",
                &[array, Value::int32(3), index_descriptor]
            )
            .is_err()
        );
        let unlock = object(&mut machine);
        machine
            .set_data_property(unlock, "writable", Value::TRUE)
            .unwrap();
        assert!(call_object(&mut machine, "defineProperty", &[array, length_key, unlock]).is_err());
    }

    #[test]
    fn array_index_definitions_update_length_atomically() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(&mut machine, Vec::new()).unwrap();
        let accessor = object(&mut machine);
        let getter = machine.intrinsics.global("Object").unwrap();
        machine.set_data_property(accessor, "get", getter).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[array, Value::int32(3), accessor],
        )
        .unwrap();
        assert_eq!(machine.array_elements(array).unwrap().unwrap().len(), 4);
        assert!(matches!(
            machine
                .own_descriptor(array, &PropertyKey::Named(EcmaString::from_utf8("3")))
                .unwrap(),
            Some(Property::Accessor { .. })
        ));

        let lock = object(&mut machine);
        machine
            .set_data_property(lock, "writable", Value::FALSE)
            .unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::from_utf8("length")).unwrap();
        call_object(&mut machine, "defineProperty", &[array, length_key, lock]).unwrap();
        let blocked_accessor = object(&mut machine);
        machine
            .set_data_property(blocked_accessor, "get", getter)
            .unwrap();
        assert!(
            call_object(
                &mut machine,
                "defineProperty",
                &[array, Value::int32(4), blocked_accessor],
            )
            .is_err()
        );
        assert_eq!(machine.array_elements(array).unwrap().unwrap().len(), 4);

        let fixed = allocate_array(&mut machine, Vec::new()).unwrap();
        let fixed_index = machine.runtime_slot(fixed).unwrap().unwrap();
        let HeapEntry::Array { extensible, .. } = &mut machine.heap[fixed_index] else {
            unreachable!("allocate_array returns an array");
        };
        *extensible = false;
        let descriptor = data_descriptor(&mut machine, Value::int32(1));
        assert!(
            call_object(
                &mut machine,
                "defineProperty",
                &[fixed, Value::int32(2), descriptor],
            )
            .is_err()
        );
        assert!(machine.array_elements(fixed).unwrap().unwrap().is_empty());
    }

    #[test]
    fn define_properties_rejects_later_invalid_getter_without_mutating_target() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        machine
            .set_data_property(target, "stable", Value::int32(9))
            .unwrap();
        let descriptors = object(&mut machine);
        let first = data_descriptor(&mut machine, Value::int32(1));
        let second = object(&mut machine);
        machine
            .set_data_property(second, "get", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(descriptors, "first", first)
            .unwrap();
        machine
            .set_data_property(descriptors, "second", second)
            .unwrap();

        assert!(call_define_properties(&mut machine, target, descriptors).is_err());

        assert_unchanged(&mut machine, target);
    }

    #[test]
    fn define_properties_propagates_later_throwing_conversion_without_mutating_target() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        machine
            .set_data_property(target, "stable", Value::int32(9))
            .unwrap();
        let descriptors = object(&mut machine);
        let first = data_descriptor(&mut machine, Value::int32(1));
        let second = object(&mut machine);
        let object_constructor = machine.intrinsics.global("Object").unwrap();
        let throwing_getter = machine
            .get_named_property(object_constructor, "defineProperty")
            .unwrap();
        machine
            .define_descriptor(
                second,
                PropertyKey::Named(EcmaString::from_utf8("get")),
                Property::Accessor {
                    getter: Some(throwing_getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        machine
            .set_data_property(descriptors, "first", first)
            .unwrap();
        machine
            .set_data_property(descriptors, "second", second)
            .unwrap();

        assert!(call_define_properties(&mut machine, target, descriptors).is_err());

        assert_unchanged(&mut machine, target);
    }

    #[test]
    fn define_properties_applies_collected_descriptors_in_enumeration_order() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        let descriptors = object(&mut machine);
        let first = data_descriptor(&mut machine, Value::int32(1));
        let second = data_descriptor(&mut machine, Value::int32(2));
        machine
            .set_data_property(descriptors, "first", first)
            .unwrap();
        machine
            .set_data_property(descriptors, "second", second)
            .unwrap();

        assert_eq!(
            call_define_properties(&mut machine, target, descriptors).unwrap(),
            target
        );

        assert_eq!(
            machine.enumerable_keys(target).unwrap(),
            vec![
                EcmaString::from_utf8("first"),
                EcmaString::from_utf8("second")
            ]
        );
        assert_eq!(
            machine.get_named_property(target, "first").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(target, "second").unwrap(),
            Value::int32(2)
        );
    }

    #[test]
    fn array_length_descriptor_reads_inherited_fields() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::int32(2), Value::int32(3)],
        )
        .unwrap();
        let prototype = object(&mut machine);
        machine
            .set_data_property(prototype, "value", Value::int32(1))
            .unwrap();
        let descriptor = call_object(&mut machine, "create", &[prototype]).unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::from_utf8("length")).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[array, length_key, descriptor],
        )
        .unwrap();

        assert_eq!(
            machine.array_elements(array).unwrap().unwrap(),
            vec![Value::int32(1)]
        );
    }

    #[test]
    fn define_properties_converts_each_descriptor_once() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        let descriptors = object(&mut machine);
        let descriptor =
            allocate_array(&mut machine, vec![Value::int32(1), Value::int32(2)]).unwrap();
        let array_prototype = machine.intrinsics.array_prototype;
        let pop = machine.get_named_property(array_prototype, "pop").unwrap();
        machine
            .define_descriptor(
                descriptor,
                PropertyKey::Named(EcmaString::from_utf8("value")),
                Property::Accessor {
                    getter: Some(pop),
                    setter: None,
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        machine
            .set_data_property(descriptors, "answer", descriptor)
            .unwrap();

        call_define_properties(&mut machine, target, descriptors).unwrap();

        assert_eq!(
            machine.get_named_property(target, "answer").unwrap(),
            Value::int32(2)
        );
        assert_eq!(
            machine.array_elements(descriptor).unwrap().unwrap(),
            vec![Value::int32(1)]
        );
    }

    #[test]
    fn partial_redefinitions_preserve_omitted_fields() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = object(&mut machine);
        machine
            .set_data_property(target, "data", Value::int32(7))
            .unwrap();
        let data_descriptor = object(&mut machine);
        machine
            .set_data_property(data_descriptor, "writable", Value::FALSE)
            .unwrap();
        let data_key = allocate_string(&mut machine, EcmaString::from_utf8("data")).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[target, data_key, data_descriptor],
        )
        .unwrap();

        assert!(matches!(
            machine
                .own_descriptor(target, &PropertyKey::Named(EcmaString::from_utf8("data")))
                .unwrap(),
            Some(Property::Data {
                value,
                writable: false,
                enumerable: true,
                configurable: true,
            }) if value == Value::int32(7)
        ));

        let getter = machine.intrinsics.global("Object").unwrap();
        let setter = machine.intrinsics.global("Array").unwrap();
        machine
            .define_descriptor(
                target,
                PropertyKey::Named(EcmaString::from_utf8("accessor")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: Some(setter),
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let accessor_descriptor = object(&mut machine);
        machine
            .set_data_property(accessor_descriptor, "set", Value::UNDEFINED)
            .unwrap();
        let accessor_key =
            allocate_string(&mut machine, EcmaString::from_utf8("accessor")).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[target, accessor_key, accessor_descriptor],
        )
        .unwrap();

        assert!(matches!(
            machine
                .own_descriptor(target, &PropertyKey::Named(EcmaString::from_utf8("accessor")))
                .unwrap(),
            Some(Property::Accessor {
                getter: Some(actual_getter),
                setter: None,
                enumerable: true,
                configurable: true,
            }) if actual_getter == getter
        ));
    }

    #[test]
    fn shrinking_array_length_processes_descriptor_indices() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter = machine.intrinsics.global("Object").unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::from_utf8("length")).unwrap();

        let array = allocate_array(&mut machine, Vec::new()).unwrap();
        let configurable_index = object(&mut machine);
        machine
            .set_data_property(configurable_index, "get", getter)
            .unwrap();
        machine
            .set_data_property(configurable_index, "configurable", Value::TRUE)
            .unwrap();
        call_object(
            &mut machine,
            "defineProperty",
            &[array, Value::int32(3), configurable_index],
        )
        .unwrap();
        let shrink = object(&mut machine);
        machine
            .set_data_property(shrink, "value", Value::int32(0))
            .unwrap();

        call_object(&mut machine, "defineProperty", &[array, length_key, shrink]).unwrap();

        assert!(machine.array_elements(array).unwrap().unwrap().is_empty());
        assert!(
            machine
                .own_descriptor(array, &PropertyKey::Named(EcmaString::from_utf8("3")))
                .unwrap()
                .is_none()
        );

        let blocked = allocate_array(&mut machine, Vec::new()).unwrap();
        let fixed_index = object(&mut machine);
        machine
            .set_data_property(fixed_index, "get", getter)
            .unwrap();
        call_object(
            &mut machine,
            "defineProperty",
            &[blocked, Value::int32(3), fixed_index],
        )
        .unwrap();
        let blocked_shrink = object(&mut machine);
        machine
            .set_data_property(blocked_shrink, "value", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(blocked_shrink, "writable", Value::FALSE)
            .unwrap();

        assert!(
            call_object(
                &mut machine,
                "defineProperty",
                &[blocked, length_key, blocked_shrink],
            )
            .is_err()
        );
        assert_eq!(machine.array_elements(blocked).unwrap().unwrap().len(), 4);
        assert!(matches!(
            machine
                .own_descriptor(blocked, &PropertyKey::Named(EcmaString::from_utf8("3")))
                .unwrap(),
            Some(Property::Accessor {
                configurable: false,
                ..
            })
        ));
        assert!(matches!(
            machine
                .own_descriptor(blocked, &PropertyKey::Named(EcmaString::from_utf8("length")))
                .unwrap(),
            Some(Property::Data {
                value,
                writable: false,
                ..
            }) if value == crate::number_value(4.0)
        ));
    }
}
