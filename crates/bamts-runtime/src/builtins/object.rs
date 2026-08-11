use std::collections::BTreeMap;

use bamts_bytecode::{EcmaString, EcmaStringBuilder, IteratorKind};
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, range_error,
    to_integer_or_infinity, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{
    BoundCallable, CollectionKind, EvalFailure, HeapEntry, Host, Machine, NativeCallable, Property,
    PropertyKey, PropertyMap, RuntimeErrorKind,
};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.object_prototype();
    let constructor = install_function(heap, builtins, "Object", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::encode("Object"), constructor);

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
    for (name, length, handler) in [
        ("call", 1, function_call::<H> as BuiltinHandler<H>),
        ("apply", 2, function_apply::<H>),
        ("bind", 1, function_bind::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, builtins.function_prototype(), name, function);
    }
}

fn machine_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let index = super::heap_index(constructor);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[index] else {
        panic!("constructor must be native");
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode(name)),
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
    let new_target = machine.current_new_target();
    let object_constructor = machine
        .intrinsics
        .global("Object")
        .expect("Object intrinsic");
    // ECMA-262 §20.1.1.1: distinct NewTarget allocates a fresh ordinary object
    // before any argument return / ToObject boxing path. Primitive NewTarget
    // must propagate constructed_prototype's InvalidValue (no unwrap_or fallback);
    // fallback to %Object.prototype% remains inside constructed_prototype for a
    // valid constructor whose [["prototype"]] is missing or non-object.
    if new_target != Value::UNDEFINED && new_target != object_constructor {
        let prototype = machine
            .constructed_prototype(new_target)
            .map_err(EvalFailure::Runtime)?;
        let value = machine
            .allocate_constructed_receiver_with(prototype)
            .map_err(EvalFailure::Runtime)?;
        return Ok(BuiltinOutcome::Value(value));
    }
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
        for key in machine.own_property_keys(source)? {
            if !machine.own_property_is_enumerable(source, &key)? {
                continue;
            }
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
    let key = PropertyKey::Named(EcmaString::encode(name));
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
    validate_property_descriptor(machine, current.as_ref(), descriptor)?;
    machine.define_descriptor(target, key, descriptor.into_property(current))
}

/// ECMA-262 §7.2.10 SameValue(x, y).
///
/// Like SameValueZero except that `+0` and `-0` are distinct.
fn same_value<H: Host>(machine: &Machine<'_, H>, left: Value, right: Value) -> bool {
    match (left.decode(), right.decode()) {
        (Some(Decoded::Number(a)), Some(Decoded::Number(b))) => {
            if a.is_nan() && b.is_nan() {
                return true;
            }
            if a == 0.0 && b == 0.0 {
                return a.is_sign_positive() == b.is_sign_positive();
            }
            a == b
        }
        (Some(Decoded::Number(a)), Some(Decoded::Int32(b)))
        | (Some(Decoded::Int32(b)), Some(Decoded::Number(a))) => {
            // The Int32 payload is a two's-complement u32; interpret it as a
            // signed i32 before converting to f64 so negative values compare
            // to their mathematical Number equivalents.
            let b_f64 = f64::from(b as i32);
            // b_f64 is always finite — never NaN. Only a can be NaN, and NaN
            // compares unequal to every finite f64, so SameValue(NaN, <int>)
            // correctly falls through to false without a dedicated guard. The
            // +0/-0 split still matters: Int32(0) maps to +0.0, so a negative
            // zero must not match it.
            if a == 0.0 && b_f64 == 0.0 {
                return a.is_sign_positive();
            }
            a == b_f64
        }
        _ => machine.same_value_zero(left, right),
    }
}

/// ECMA-262 §10.1.6.3 ValidateAndApplyPropertyDescriptor — validation half.
///
/// When `current` is absent the property is being created and any descriptor is
/// accepted. When `current` is configurable any change is permitted. Otherwise
/// the spec forbids: making the property configurable, changing enumerability,
/// converting between data and accessor form, making a non-writable data
/// property writable, or changing the value of a non-writable data property
/// (using SameValue). A no-op redefinition that changes nothing is allowed.
fn validate_property_descriptor<H: Host>(
    machine: &Machine<'_, H>,
    current: Option<&Property>,
    descriptor: PropertyDescriptor,
) -> Result<(), EvalFailure> {
    let Some(current) = current else {
        return Ok(());
    };
    if current.configurable() {
        return Ok(());
    }
    if descriptor.configurable == Some(true) {
        return Err(type_error(
            "Cannot redefine property: non-configurable property cannot be made configurable",
        ));
    }
    if descriptor
        .enumerable
        .is_some_and(|enumerable| enumerable != current.enumerable())
    {
        return Err(type_error(
            "Cannot redefine property: cannot change enumerability of non-configurable property",
        ));
    }
    match current {
        Property::Data {
            writable, value, ..
        } => {
            if descriptor.is_accessor() {
                return Err(type_error(
                    "Cannot redefine property: cannot convert non-configurable data property to accessor",
                ));
            }
            if !*writable {
                if descriptor.writable == Some(true) {
                    return Err(type_error(
                        "Cannot redefine property: cannot make non-writable property writable",
                    ));
                }
                if descriptor
                    .value
                    .is_some_and(|next| !same_value(machine, next, *value))
                {
                    return Err(type_error(
                        "Cannot redefine property: cannot change value of non-writable property",
                    ));
                }
            }
        }
        Property::Accessor { getter, setter, .. } => {
            if descriptor.is_data() {
                return Err(type_error(
                    "Cannot redefine property: cannot convert non-configurable accessor property to data",
                ));
            }
            if descriptor
                .getter
                .is_some_and(|next| !same_value(machine, next, getter.unwrap_or(Value::UNDEFINED)))
            {
                return Err(type_error(
                    "Cannot redefine property: cannot change getter of non-configurable accessor property",
                ));
            }
            if descriptor
                .setter
                .is_some_and(|next| !same_value(machine, next, setter.unwrap_or(Value::UNDEFINED)))
            {
                return Err(type_error(
                    "Cannot redefine property: cannot change setter of non-configurable accessor property",
                ));
            }
        }
    }
    Ok(())
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
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.object_prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    let depth = machine.native_roots.len();
    machine.push_native_roots(depth, &[object, iterable]);
    let result: Result<(), EvalFailure> = (|| {
        let iterator = machine.create_iterator(iterable, IteratorKind::Sync)?;
        machine.refresh_native_roots(depth, &[object, iterator]);
        loop {
            let (done, entry) = machine.iterator_next(iterator)?;
            if done {
                break;
            }
            machine.refresh_native_roots(depth, &[object, iterator, entry]);
            let inserted = (|| {
                if !machine.is_object(entry) {
                    return Err(type_error("Iterator value is not an entry object"));
                }
                let key_value = machine.get_named_property(entry, "0")?;
                machine.refresh_native_roots(depth, &[object, iterator, entry, key_value]);
                let key = machine.to_property_key(key_value)?;
                machine.refresh_native_roots(depth, &[object, iterator, entry, key_value]);
                let value = machine.get_named_property(entry, "1")?;
                machine.refresh_native_roots(depth, &[object, iterator, entry, key_value, value]);
                machine.create_data_property_key(object, key, value)
            })();
            if let Err(failure) = inserted {
                match &failure {
                    EvalFailure::ThrowValue(value)
                    | EvalFailure::ThrowValueOrigin { value, .. } => {
                        machine.refresh_native_roots(depth, &[object, iterator, *value]);
                    }
                    EvalFailure::Throw(_) | EvalFailure::Runtime(_) => {
                        machine.refresh_native_roots(depth, &[object, iterator]);
                    }
                }
                return Err(super::collections::close_iterator_preserving_failure(
                    machine, iterator, failure,
                ));
            }
        }
        Ok(())
    })();
    machine.pop_native_roots(depth);
    result?;
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
        Some(Decoded::Undefined) => EcmaString::encode("Undefined"),
        Some(Decoded::Null) => EcmaString::encode("Null"),
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
                .unwrap_or_else(|| EcmaString::encode(fallback))
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
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error("Function.prototype.call is not a constructor"));
    }
    Ok(BuiltinOutcome::Call {
        callee: this,
        this_value: args.first().copied().unwrap_or(Value::UNDEFINED),
        arguments: args.get(1..).unwrap_or_default().to_vec(),
    })
}

fn function_apply<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error("Function.prototype.apply is not a constructor"));
    }
    if !machine.is_callable(this)? {
        return Err(type_error(
            "Function.prototype.apply receiver is not callable",
        ));
    }
    let this_value = args.first().copied().unwrap_or(Value::UNDEFINED);
    let source = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let arguments = if matches!(source.decode(), Some(Decoded::Undefined | Decoded::Null)) {
        Vec::new()
    } else {
        create_list_from_array_like(machine, source)?
    };
    Ok(BuiltinOutcome::Call {
        callee: this,
        this_value,
        arguments,
    })
}

fn create_list_from_array_like<H: Host>(
    machine: &mut Machine<'_, H>,
    source: Value,
) -> Result<Vec<Value>, EvalFailure> {
    if !machine.is_object(source) {
        return Err(type_error(
            "Function.prototype.apply arguments are not an object",
        ));
    }
    let length_value = machine.get_named_property(source, "length")?;
    let number = value_number(machine.coerce_number_observable(length_value)?);
    let length = (if number.is_nan() || number == 0.0 {
        0.0
    } else {
        number.trunc()
    })
    .clamp(0.0, 9_007_199_254_740_991.0);
    if length > f64::from(machine.limits.max_argument_count) {
        return Err(EvalFailure::Runtime(
            RuntimeErrorKind::ArgumentLimitExceeded {
                limit: machine.limits.max_argument_count,
                requested: length.min(f64::from(u32::MAX)) as u32,
            },
        ));
    }
    let length = length as usize;
    let mut arguments = Vec::with_capacity(length);
    for index in 0..length {
        let key = PropertyKey::Named(EcmaString::encode(&index.to_string()));
        arguments.push(machine.get_property_key(source, &key)?);
    }
    Ok(arguments)
}

fn function_bind<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error("Function.prototype.bind is not a constructor"));
    }
    if !machine.is_callable(this)? {
        return Err(type_error(
            "Function.prototype.bind receiver is not callable",
        ));
    }
    let bound_arguments = args.get(1..).unwrap_or_default().to_vec();
    let length_key = PropertyKey::Named(EcmaString::encode("length"));
    let length = if machine.has_own_property_key(this, &length_key)? {
        let target_length = machine.get_property_key(this, &length_key)?;
        match target_length.decode() {
            Some(Decoded::Number(_) | Decoded::Int32(_)) => {
                let value = to_integer_or_infinity(machine, target_length)?;
                if value == f64::INFINITY {
                    value
                } else {
                    (value.max(0.0) - bound_arguments.len() as f64).max(0.0)
                }
            }
            _ => 0.0,
        }
    } else {
        0.0
    };
    let target_name = machine.get_named_property(this, "name")?;
    let target_name = machine
        .string_value(target_name)
        .unwrap_or_else(|| EcmaString::encode(""));
    let mut name = EcmaStringBuilder::with_capacity(target_name.len_units().saturating_add(6));
    name.push_utf8("bound ");
    for unit in target_name.as_units() {
        name.push_unit(*unit);
    }
    let name = allocate_string(machine, name.finish())?;
    let mut properties = PropertyMap::default();
    properties.insert(
        length_key,
        Property::Data {
            value: crate::number_value(length),
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        PropertyKey::Named(EcmaString::encode("name")),
        Property::Data {
            value: name,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    let bound_prototype = machine.prototype_value(this)?;
    let value = machine
        .allocate(HeapEntry::native_function(
            NativeCallable::Bound(Box::new(BoundCallable {
                target: this,
                this_value: args.first().copied().unwrap_or(Value::UNDEFINED),
                arguments: bound_arguments,
            })),
            properties,
            bound_prototype,
        ))
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(value))
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
    if matches!(
        &machine.heap[index],
        HeapEntry::Collection {
            kind: CollectionKind::WeakMap | CollectionKind::WeakSet,
            ..
        }
    ) {
        return Err(type_error("value could not be cloned"));
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
        }
        | HeapEntry::Script {
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
        HeapEntry::Date {
            time, prototype, ..
        } => {
            let clone = machine
                .allocate(HeapEntry::Date {
                    time,
                    properties: PropertyMap::default(),
                    prototype,
                    extensible: true,
                })
                .map_err(EvalFailure::Runtime)?;
            seen.insert(index, clone);
            Ok(clone)
        }
        HeapEntry::Collection {
            kind,
            entries,
            prototype,
            ..
        } => {
            let clone = machine
                .allocate(HeapEntry::Collection {
                    kind,
                    entries: Vec::new(),
                    index: crate::CollectionIndex::default(),
                    size: 0,
                    next_order: 0,
                    properties: PropertyMap::default(),
                    prototype,
                    extensible: true,
                })
                .map_err(EvalFailure::Runtime)?;
            seen.insert(index, clone);
            let clone_index = machine.runtime_slot(clone).unwrap().unwrap();
            for entry in entries.into_iter().filter(|entry| entry.live) {
                let key = clone_value(machine, entry.key, seen)?;
                let value = clone_value(machine, entry.value, seen)?;
                super::collections::append_collection_entry(machine, clone_index, key, value)?;
            }
            Ok(clone)
        }
        HeapEntry::Uint8Array {
            bytes,
            properties,
            prototype,
            extensible,
        } => {
            let clone = machine
                .allocate(HeapEntry::Uint8Array {
                    bytes,
                    properties: PropertyMap::default(),
                    prototype,
                    extensible,
                })
                .map_err(EvalFailure::Runtime)?;
            seen.insert(index, clone);
            for (key, property) in properties.0 {
                if key.eq_ascii("length")
                    || key
                        .as_string()
                        .is_some_and(|name| crate::uint8array_index(name).is_some())
                {
                    continue;
                }
                if let Property::Data { value, .. } = property {
                    let copied = clone_value(machine, value, seen)?;
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
    use super::super::test_support::{TestHost, blank_program, ordinary_object};
    use super::*;
    use crate::{Limits, RuntimeErrorKind, ThrowOrigin};
    use bamts_bytecode::{FunctionId, ModuleId};

    fn data_descriptor(machine: &mut Machine<'_, TestHost>, value: Value) -> Value {
        let descriptor = ordinary_object(machine);
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
                .has_own_property_key(target, &PropertyKey::Named(EcmaString::encode("first")))
                .unwrap()
        );
        assert!(
            !machine
                .has_own_property_key(target, &PropertyKey::Named(EcmaString::encode("second")))
                .unwrap()
        );
    }

    fn symbol_key(machine: &Machine<'_, TestHost>, symbol: Value) -> PropertyKey {
        machine.to_property_key(symbol).unwrap()
    }

    fn symbol(machine: &mut Machine<'_, TestHost>, description: &str) -> Value {
        machine
            .allocate(HeapEntry::Symbol {
                description: EcmaString::encode(description),
            })
            .unwrap()
    }

    #[test]
    fn object_reflection_preserves_symbol_keys_and_filters_string_names() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let key = symbol(&mut machine, "key");
        let descriptor = data_descriptor(&mut machine, Value::int32(42));

        call_object(&mut machine, "defineProperty", &[target, key, descriptor]).unwrap();

        let property_key = symbol_key(&machine, key);
        assert_eq!(
            machine.get_property_key(target, &property_key).unwrap(),
            Value::int32(42)
        );
        let child = ordinary_object(&mut machine);
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
    fn assign_copies_enumerable_symbol_properties() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let source = ordinary_object(&mut machine);
        let symbol = symbol(&mut machine, "key");
        let key = symbol_key(&machine, symbol);
        machine
            .set_data_property_key(source, key.clone(), Value::int32(42))
            .unwrap();

        call_object(&mut machine, "assign", &[target, source]).unwrap();

        assert_eq!(
            machine.get_property_key(target, &key).unwrap(),
            Value::int32(42)
        );
    }

    #[test]
    fn assign_rechecks_descriptors_after_getters() {
        fn delete_next<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            machine.delete_property(this, &PropertyKey::Named(EcmaString::encode("next")))?;
            Ok(BuiltinOutcome::Value(Value::int32(1)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "delete next",
                length: 0,
                handler: delete_next::<TestHost>,
            });
        let getter =
            crate::intrinsics::native_function(&mut machine.heap, getter_id, "delete next", 0);
        let source = ordinary_object(&mut machine);
        let target = ordinary_object(&mut machine);
        let first = PropertyKey::Named(EcmaString::encode("first"));
        let next = PropertyKey::Named(EcmaString::encode("next"));
        machine
            .define_descriptor(
                source,
                first.clone(),
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        machine
            .set_data_property_key(source, next.clone(), Value::int32(2))
            .unwrap();

        call_object(&mut machine, "assign", &[target, source]).unwrap();

        assert_eq!(
            machine.get_property_key(target, &first).unwrap(),
            Value::int32(1)
        );
        assert!(!machine.has_own_property_key(target, &next).unwrap());
    }

    #[test]
    fn assign_orders_descriptor_backed_array_indices_numerically() {
        fn delete_later_index<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            machine.delete_property(this, &PropertyKey::Named(EcmaString::encode("10")))?;
            Ok(BuiltinOutcome::Value(Value::int32(1)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "delete later index",
                length: 0,
                handler: delete_later_index::<TestHost>,
            });
        let getter = crate::intrinsics::native_function(
            &mut machine.heap,
            getter_id,
            "delete later index",
            0,
        );
        let mut elements = vec![Value::HOLE; 11];
        elements[10] = Value::int32(2);
        let source = allocate_array(&mut machine, elements).unwrap();
        machine
            .define_descriptor(
                source,
                PropertyKey::Named(EcmaString::encode("2")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let target = ordinary_object(&mut machine);
        let later = PropertyKey::Named(EcmaString::encode("10"));

        call_object(&mut machine, "assign", &[target, source]).unwrap();

        assert!(!machine.has_own_property_key(target, &later).unwrap());
    }

    #[test]
    fn define_properties_collects_enumerable_symbol_descriptors() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let descriptors = ordinary_object(&mut machine);
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
    fn define_properties_ignores_language_private_descriptors() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let descriptors = ordinary_object(&mut machine);
        let private = machine
            .allocate(HeapEntry::PrivateName {
                description: EcmaString::encode("private"),
            })
            .unwrap();
        let key = machine.to_property_key(private).unwrap();
        let descriptor = data_descriptor(&mut machine, Value::int32(7));
        machine
            .set_data_property_key(descriptors, key.clone(), descriptor)
            .unwrap();

        call_define_properties(&mut machine, target, descriptors).unwrap();

        assert_eq!(
            machine.get_property_key(target, &key).unwrap(),
            Value::UNDEFINED
        );
    }

    #[test]
    fn existing_namespaces_expose_standard_to_string_tags() {
        let module = blank_program("<test>");
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
        let module = blank_program("<test>");
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let tag_key = symbol_key(&machine, machine.intrinsics.builtins.symbol_to_string_tag());
        let object_to_string = machine.intrinsics.object_to_string();

        let non_string_tag = ordinary_object(&mut machine);
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

        let accessor_tag = ordinary_object(&mut machine);
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let symbol = symbol(&mut machine, "public");
        let private = machine
            .allocate(HeapEntry::PrivateName {
                description: EcmaString::encode("private"),
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
        let module = blank_program("<test>");
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
            .own_descriptor(array, &PropertyKey::Named(EcmaString::encode("length")))
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

        let locked = ordinary_object(&mut machine);
        machine
            .set_data_property(locked, "writable", Value::FALSE)
            .unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::encode("length")).unwrap();
        call_object(&mut machine, "defineProperty", &[array, length_key, locked]).unwrap();
        let same_length = ordinary_object(&mut machine);
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
        let unlock = ordinary_object(&mut machine);
        machine
            .set_data_property(unlock, "writable", Value::TRUE)
            .unwrap();
        assert!(call_object(&mut machine, "defineProperty", &[array, length_key, unlock]).is_err());
    }

    #[test]
    fn array_index_definitions_update_length_atomically() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(&mut machine, Vec::new()).unwrap();
        let accessor = ordinary_object(&mut machine);
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
                .own_descriptor(array, &PropertyKey::Named(EcmaString::encode("3")))
                .unwrap(),
            Some(Property::Accessor { .. })
        ));

        let lock = ordinary_object(&mut machine);
        machine
            .set_data_property(lock, "writable", Value::FALSE)
            .unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::encode("length")).unwrap();
        call_object(&mut machine, "defineProperty", &[array, length_key, lock]).unwrap();
        let blocked_accessor = ordinary_object(&mut machine);
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        machine
            .set_data_property(target, "stable", Value::int32(9))
            .unwrap();
        let descriptors = ordinary_object(&mut machine);
        let first = data_descriptor(&mut machine, Value::int32(1));
        let second = ordinary_object(&mut machine);
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        machine
            .set_data_property(target, "stable", Value::int32(9))
            .unwrap();
        let descriptors = ordinary_object(&mut machine);
        let first = data_descriptor(&mut machine, Value::int32(1));
        let second = ordinary_object(&mut machine);
        let object_constructor = machine.intrinsics.global("Object").unwrap();
        let throwing_getter = machine
            .get_named_property(object_constructor, "defineProperty")
            .unwrap();
        machine
            .define_descriptor(
                second,
                PropertyKey::Named(EcmaString::encode("get")),
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let descriptors = ordinary_object(&mut machine);
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
            vec![EcmaString::encode("first"), EcmaString::encode("second")]
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::int32(2), Value::int32(3)],
        )
        .unwrap();
        let prototype = ordinary_object(&mut machine);
        machine
            .set_data_property(prototype, "value", Value::int32(1))
            .unwrap();
        let descriptor = call_object(&mut machine, "create", &[prototype]).unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::encode("length")).unwrap();

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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let descriptors = ordinary_object(&mut machine);
        let descriptor =
            allocate_array(&mut machine, vec![Value::int32(1), Value::int32(2)]).unwrap();
        let array_prototype = machine.intrinsics.array_prototype;
        let pop = machine.get_named_property(array_prototype, "pop").unwrap();
        machine
            .define_descriptor(
                descriptor,
                PropertyKey::Named(EcmaString::encode("value")),
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        machine
            .set_data_property(target, "data", Value::int32(7))
            .unwrap();
        let data_descriptor = ordinary_object(&mut machine);
        machine
            .set_data_property(data_descriptor, "writable", Value::FALSE)
            .unwrap();
        let data_key = allocate_string(&mut machine, EcmaString::encode("data")).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[target, data_key, data_descriptor],
        )
        .unwrap();

        assert!(matches!(
            machine
                .own_descriptor(target, &PropertyKey::Named(EcmaString::encode("data")))
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
                PropertyKey::Named(EcmaString::encode("accessor")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: Some(setter),
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let accessor_descriptor = ordinary_object(&mut machine);
        machine
            .set_data_property(accessor_descriptor, "set", Value::UNDEFINED)
            .unwrap();
        let accessor_key = allocate_string(&mut machine, EcmaString::encode("accessor")).unwrap();

        call_object(
            &mut machine,
            "defineProperty",
            &[target, accessor_key, accessor_descriptor],
        )
        .unwrap();

        assert!(matches!(
            machine
                .own_descriptor(target, &PropertyKey::Named(EcmaString::encode("accessor")))
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter = machine.intrinsics.global("Object").unwrap();
        let length_key = allocate_string(&mut machine, EcmaString::encode("length")).unwrap();

        let array = allocate_array(&mut machine, Vec::new()).unwrap();
        let configurable_index = ordinary_object(&mut machine);
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
        let shrink = ordinary_object(&mut machine);
        machine
            .set_data_property(shrink, "value", Value::int32(0))
            .unwrap();

        call_object(&mut machine, "defineProperty", &[array, length_key, shrink]).unwrap();

        assert!(machine.array_elements(array).unwrap().unwrap().is_empty());
        assert!(
            machine
                .own_descriptor(array, &PropertyKey::Named(EcmaString::encode("3")))
                .unwrap()
                .is_none()
        );

        let blocked = allocate_array(&mut machine, Vec::new()).unwrap();
        let fixed_index = ordinary_object(&mut machine);
        machine
            .set_data_property(fixed_index, "get", getter)
            .unwrap();
        call_object(
            &mut machine,
            "defineProperty",
            &[blocked, Value::int32(3), fixed_index],
        )
        .unwrap();
        let blocked_shrink = ordinary_object(&mut machine);
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
                .own_descriptor(blocked, &PropertyKey::Named(EcmaString::encode("3")))
                .unwrap(),
            Some(Property::Accessor {
                configurable: false,
                ..
            })
        ));
        assert!(matches!(
            machine
                .own_descriptor(blocked, &PropertyKey::Named(EcmaString::encode("length")))
                .unwrap(),
            Some(Property::Data {
                value,
                writable: false,
                ..
            }) if value == crate::number_value(4.0)
        ));
    }
    fn probe_handler<H: Host>(
        machine: &mut Machine<'_, H>,
        this: Value,
        args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let mut values = Vec::with_capacity(args.len() + 1);
        values.push(this);
        values.extend_from_slice(args);
        Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
    }

    fn probe(machine: &mut Machine<'_, TestHost>, name: &'static str, length: u32) -> Value {
        let id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name,
                length,
                handler: probe_handler::<TestHost>,
            });
        crate::intrinsics::native_function(&mut machine.heap, id, name, length)
    }

    fn call_method(
        machine: &mut Machine<'_, TestHost>,
        target: Value,
        name: &str,
        args: &[Value],
    ) -> Result<Value, EvalFailure> {
        let method = machine.get_named_property(target, name)?;
        machine.call_value(method, target, args)
    }

    #[test]
    fn apply_forwards_array_like_arguments_and_receiver() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let arguments = ordinary_object(&mut machine);
        machine
            .set_data_property(arguments, "length", Value::int32(2))
            .unwrap();
        machine
            .set_data_property(arguments, "0", Value::int32(11))
            .unwrap();
        machine
            .set_data_property(arguments, "1", Value::int32(22))
            .unwrap();

        let array = allocate_array(&mut machine, vec![Value::int32(11), Value::int32(22)]).unwrap();
        let array_result = call_method(&mut machine, target, "apply", &[receiver, array]).unwrap();
        assert_eq!(
            machine.array_elements(array_result).unwrap().unwrap(),
            vec![receiver, Value::int32(11), Value::int32(22)]
        );

        let result = call_method(&mut machine, target, "apply", &[receiver, arguments]).unwrap();

        assert_eq!(
            machine.array_elements(result).unwrap().unwrap(),
            vec![receiver, Value::int32(11), Value::int32(22)]
        );
        let empty =
            call_method(&mut machine, target, "apply", &[receiver, Value::UNDEFINED]).unwrap();
        assert_eq!(
            machine.array_elements(empty).unwrap().unwrap(),
            vec![receiver]
        );
        let empty = call_method(&mut machine, target, "apply", &[receiver, Value::NULL]).unwrap();
        assert_eq!(
            machine.array_elements(empty).unwrap().unwrap(),
            vec![receiver]
        );
        for primitive in [
            machine
                .allocate(HeapEntry::String(EcmaString::encode("not array-like")))
                .unwrap(),
            Value::int32(1),
            Value::TRUE,
        ] {
            assert!(matches!(
                call_method(&mut machine, target, "apply", &[receiver, primitive]),
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ));
        }
    }

    #[test]
    fn apply_reads_length_once_then_indices_in_order() {
        fn length_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "reads")? != Value::int32(0) {
                return Err(type_error("length was read more than once"));
            }
            machine.set_data_property(this, "reads", Value::int32(1))?;
            machine.define_descriptor(
                this,
                PropertyKey::Named(EcmaString::encode("length")),
                Property::Data {
                    value: Value::int32(0),
                    writable: true,
                    enumerable: false,
                    configurable: true,
                },
            )?;
            Ok(BuiltinOutcome::Value(Value::int32(2)))
        }

        fn index_zero_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "next")? != Value::int32(0) {
                return Err(type_error("array-like indices were read out of order"));
            }
            machine.set_data_property(this, "next", Value::int32(1))?;
            Ok(BuiltinOutcome::Value(Value::int32(11)))
        }

        fn index_one_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "next")? != Value::int32(1) {
                return Err(type_error("array-like indices were read out of order"));
            }
            machine.set_data_property(this, "next", Value::int32(2))?;
            Ok(BuiltinOutcome::Value(Value::int32(22)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let arguments = ordinary_object(&mut machine);
        machine
            .set_data_property(arguments, "reads", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(arguments, "next", Value::int32(0))
            .unwrap();
        let handlers = [
            (
                "length getter",
                length_getter::<TestHost> as BuiltinHandler<TestHost>,
            ),
            ("index 0 getter", index_zero_getter::<TestHost>),
            ("index 1 getter", index_one_getter::<TestHost>),
        ];
        for ((name, handler), key) in handlers.into_iter().zip(["length", "0", "1"]) {
            let id = machine
                .intrinsics
                .builtins
                .register(crate::intrinsics::BuiltinDef {
                    name,
                    length: 0,
                    handler,
                });
            let getter = crate::intrinsics::native_function(&mut machine.heap, id, name, 0);
            machine
                .define_descriptor(
                    arguments,
                    PropertyKey::Named(EcmaString::encode(key)),
                    Property::Accessor {
                        getter: Some(getter),
                        setter: None,
                        enumerable: true,
                        configurable: true,
                    },
                )
                .unwrap();
        }

        let result = call_method(&mut machine, target, "apply", &[receiver, arguments]).unwrap();

        assert_eq!(
            machine.array_elements(result).unwrap().unwrap(),
            vec![receiver, Value::int32(11), Value::int32(22)]
        );
        assert_eq!(
            machine.get_named_property(arguments, "reads").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(arguments, "next").unwrap(),
            Value::int32(2)
        );
    }

    #[test]
    fn apply_rejects_non_callable_before_reading_arguments() {
        fn mark_length<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            machine.set_data_property(this, "touched", Value::TRUE)?;
            Ok(BuiltinOutcome::Value(Value::int32(0)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "mark length",
                length: 0,
                handler: mark_length::<TestHost>,
            });
        let getter =
            crate::intrinsics::native_function(&mut machine.heap, getter_id, "mark length", 0);
        let arguments = ordinary_object(&mut machine);
        machine
            .define_descriptor(
                arguments,
                PropertyKey::Named(EcmaString::encode("length")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        let invalid = ordinary_object(&mut machine);
        let apply = machine
            .get_named_property(machine.intrinsics.function_prototype, "apply")
            .unwrap();

        assert!(matches!(
            machine.call_value(apply, invalid, &[Value::UNDEFINED, arguments]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert_eq!(
            machine.get_named_property(arguments, "touched").unwrap(),
            Value::UNDEFINED
        );
    }

    #[test]
    fn apply_converts_object_length_via_valueof_tostring_before_indices() {
        fn value_of<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "step")? != Value::int32(0) {
                return Err(type_error("valueOf called out of order"));
            }
            machine.set_data_property(this, "step", Value::int32(1))?;
            Ok(BuiltinOutcome::Value(this))
        }

        fn to_string<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "step")? != Value::int32(1) {
                return Err(type_error("toString called out of order"));
            }
            machine.set_data_property(this, "step", Value::int32(2))?;
            Ok(BuiltinOutcome::Value(Value::int32(2)))
        }

        fn index_zero_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "step")? != Value::int32(2) {
                return Err(type_error("index 0 read before length conversion"));
            }
            machine.set_data_property(this, "step", Value::int32(3))?;
            Ok(BuiltinOutcome::Value(Value::int32(11)))
        }

        fn index_one_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            if machine.get_named_property(this, "step")? != Value::int32(3) {
                return Err(type_error("index 1 read out of order"));
            }
            machine.set_data_property(this, "step", Value::int32(4))?;
            Ok(BuiltinOutcome::Value(Value::int32(22)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let arguments = ordinary_object(&mut machine);
        machine
            .set_data_property(arguments, "step", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(arguments, "length", arguments)
            .unwrap();

        let value_of_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "valueOf",
                length: 0,
                handler: value_of::<TestHost>,
            });
        let value_of_fn =
            crate::intrinsics::native_function(&mut machine.heap, value_of_id, "valueOf", 0);
        machine
            .set_data_property(arguments, "valueOf", value_of_fn)
            .unwrap();

        let to_string_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "toString",
                length: 0,
                handler: to_string::<TestHost>,
            });
        let to_string_fn =
            crate::intrinsics::native_function(&mut machine.heap, to_string_id, "toString", 0);
        machine
            .set_data_property(arguments, "toString", to_string_fn)
            .unwrap();

        let index_zero_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "index 0 getter",
                length: 0,
                handler: index_zero_getter::<TestHost>,
            });
        let index_zero_getter_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            index_zero_id,
            "index 0 getter",
            0,
        );
        machine
            .define_descriptor(
                arguments,
                PropertyKey::Named(EcmaString::encode("0")),
                Property::Accessor {
                    getter: Some(index_zero_getter_fn),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();

        let index_one_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "index 1 getter",
                length: 0,
                handler: index_one_getter::<TestHost>,
            });
        let index_one_getter_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            index_one_id,
            "index 1 getter",
            0,
        );
        machine
            .define_descriptor(
                arguments,
                PropertyKey::Named(EcmaString::encode("1")),
                Property::Accessor {
                    getter: Some(index_one_getter_fn),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();

        let result = call_method(&mut machine, target, "apply", &[receiver, arguments]).unwrap();
        assert_eq!(
            machine.array_elements(result).unwrap().unwrap(),
            vec![receiver, Value::int32(11), Value::int32(22)]
        );
        assert_eq!(
            machine.get_named_property(arguments, "step").unwrap(),
            Value::int32(4)
        );
    }

    #[test]
    fn apply_propagates_length_conversion_failure() {
        fn throwing_value_of<H: Host>(
            _machine: &mut Machine<'_, H>,
            _this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            Err(type_error("length valueOf throws"))
        }

        fn index_zero_getter<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            machine.set_data_property(this, "touched", Value::TRUE)?;
            Ok(BuiltinOutcome::Value(Value::int32(11)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let arguments = ordinary_object(&mut machine);
        machine
            .set_data_property(arguments, "touched", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(arguments, "length", arguments)
            .unwrap();

        let value_of_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "throwing valueOf",
                length: 0,
                handler: throwing_value_of::<TestHost>,
            });
        let value_of_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            value_of_id,
            "throwing valueOf",
            0,
        );
        machine
            .set_data_property(arguments, "valueOf", value_of_fn)
            .unwrap();

        let index_zero_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "index 0 getter",
                length: 0,
                handler: index_zero_getter::<TestHost>,
            });
        let index_zero_getter_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            index_zero_id,
            "index 0 getter",
            0,
        );
        machine
            .define_descriptor(
                arguments,
                PropertyKey::Named(EcmaString::encode("0")),
                Property::Accessor {
                    getter: Some(index_zero_getter_fn),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();

        assert!(matches!(
            call_method(&mut machine, target, "apply", &[receiver, arguments]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert_eq!(
            machine.get_named_property(arguments, "touched").unwrap(),
            Value::FALSE
        );
    }

    #[test]
    fn bind_pins_receiver_prepends_arguments_and_sets_metadata() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let ignored = ordinary_object(&mut machine);
        let no_arguments = call_method(&mut machine, target, "bind", &[]).unwrap();
        assert_eq!(
            machine.get_named_property(no_arguments, "length").unwrap(),
            crate::number_value(2.0)
        );
        let saturated = call_method(
            &mut machine,
            target,
            "bind",
            &[receiver, Value::int32(1), Value::int32(2), Value::int32(3)],
        )
        .unwrap();
        assert_eq!(
            machine.get_named_property(saturated, "length").unwrap(),
            crate::number_value(0.0)
        );
        let bound =
            call_method(&mut machine, target, "bind", &[receiver, Value::int32(1)]).unwrap();

        let result = machine
            .call_value(bound, ignored, &[Value::int32(2)])
            .unwrap();
        assert_eq!(
            machine.array_elements(result).unwrap().unwrap(),
            vec![receiver, Value::int32(1), Value::int32(2)]
        );
        assert_eq!(
            machine.get_named_property(bound, "length").unwrap(),
            crate::number_value(1.0)
        );
        let name = machine.get_named_property(bound, "name").unwrap();
        assert!(
            machine
                .string_value(name)
                .is_some_and(|name| name.eq_ascii("bound probe"))
        );
        assert_eq!(
            machine.prototype_value(bound).unwrap(),
            Some(machine.intrinsics.function_prototype)
        );
        assert!(
            !machine
                .has_own_property_key(bound, &PropertyKey::Named(EcmaString::encode("prototype")),)
                .unwrap()
        );
        assert_eq!(
            machine.own_property_keys(bound).unwrap(),
            vec![
                PropertyKey::Named(EcmaString::encode("length")),
                PropertyKey::Named(EcmaString::encode("name")),
            ]
        );

        let nested = call_method(&mut machine, bound, "bind", &[ignored, Value::int32(3)]).unwrap();
        let nested_result = machine
            .call_value(nested, Value::UNDEFINED, &[Value::int32(4)])
            .unwrap();
        assert_eq!(
            machine.array_elements(nested_result).unwrap().unwrap(),
            vec![receiver, Value::int32(1), Value::int32(3), Value::int32(4),]
        );
        let name = machine.get_named_property(nested, "name").unwrap();
        assert!(
            machine
                .string_value(name)
                .is_some_and(|name| name.eq_ascii("bound bound probe"))
        );
    }

    #[test]
    fn bound_constructor_uses_target_prototype_and_instanceof() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let prototype = ordinary_object(&mut machine);
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        let target = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let bound_this = ordinary_object(&mut machine);
        let bound =
            call_method(&mut machine, target, "bind", &[bound_this, Value::int32(1)]).unwrap();
        machine.execute_construct(bound, bound, &[], 0, 0).unwrap();
        assert_eq!(machine.frames.len(), 2);
        assert!(machine.run_loop(1).unwrap().is_none());
        let instance = machine.read_register(0, 0);

        assert_eq!(machine.prototype_value(instance).unwrap(), Some(prototype));
        assert!(machine.instance_of(instance, bound).unwrap());
        assert!(machine.instance_of(instance, target).unwrap());
    }

    #[test]
    fn object_constructor_distinct_new_target_ignores_value() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object_constructor = machine.intrinsics.global("Object").unwrap();
        let index = machine.runtime_slot(object_constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(object_id),
            ..
        } = machine.heap[index]
        else {
            panic!("Object is a builtin");
        };

        let custom_prototype = ordinary_object(&mut machine);
        machine
            .set_data_property(custom_prototype, "marker", Value::int32(42))
            .unwrap();
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: custom_prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        let custom_new_target = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();

        let existing = ordinary_object(&mut machine);
        machine
            .set_data_property(existing, "own", Value::int32(7))
            .unwrap();
        let BuiltinOutcome::Value(from_object) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &[existing],
                true,
                custom_new_target,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_ne!(from_object, existing);
        assert_eq!(
            machine.prototype_value(from_object).unwrap(),
            Some(custom_prototype)
        );
        assert!(
            !machine
                .has_own_property_key(from_object, &PropertyKey::Named(EcmaString::encode("own")))
                .unwrap()
        );
        assert_eq!(
            machine.get_named_property(from_object, "marker").unwrap(),
            Value::int32(42)
        );

        let BuiltinOutcome::Value(from_primitive) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &[Value::int32(99)],
                true,
                custom_new_target,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert!(machine.is_object(from_primitive));
        assert_eq!(
            machine.prototype_value(from_primitive).unwrap(),
            Some(custom_prototype)
        );
        // Fresh ordinary object — not a boxed Number.
        let slot = machine.runtime_slot(from_primitive).unwrap().unwrap();
        assert!(matches!(
            &machine.heap[slot],
            HeapEntry::Object {
                boxed_primitive: None,
                ..
            }
        ));

        // Ordinary active-target / call paths keep argument return and boxing.
        let BuiltinOutcome::Value(returned) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &[existing],
                true,
                object_constructor,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_eq!(returned, existing);

        let BuiltinOutcome::Value(boxed) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &[Value::int32(99)],
                true,
                object_constructor,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_ne!(boxed, from_primitive);
        let boxed_slot = machine.runtime_slot(boxed).unwrap().unwrap();
        assert!(matches!(
            &machine.heap[boxed_slot],
            HeapEntry::Object {
                boxed_primitive: Some(_),
                ..
            }
        ));

        let BuiltinOutcome::Value(called) = machine
            .call_builtin(object_id, Value::UNDEFINED, &[existing], false)
            .unwrap()
        else {
            panic!("Object call returns a value");
        };
        assert_eq!(called, existing);

        // Distinct primitive NewTarget must reject (no Object.prototype fallback).
        let before_new_target = machine.current_new_target();
        let before_builtin_id = machine.current_builtin_id();
        let primitive_nt = machine.call_builtin_with_new_target(
            object_id,
            Value::UNDEFINED,
            &[existing],
            true,
            Value::int32(99),
        );
        assert!(matches!(
            primitive_nt,
            Err(EvalFailure::Runtime(RuntimeErrorKind::InvalidValue { .. }))
        ));
        // Nested/error exit must restore ambient construct state.
        assert_eq!(machine.current_new_target(), before_new_target);
        assert_eq!(machine.current_builtin_id(), before_builtin_id);

        // Non-object constructor prototype still falls back for a valid object NewTarget.
        let mut bare_properties = PropertyMap::default();
        bare_properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: Value::int32(1),
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        let bare_new_target = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties: bare_properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();
        let BuiltinOutcome::Value(fallback) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &[existing],
                true,
                bare_new_target,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_ne!(fallback, existing);
        assert_eq!(
            machine.prototype_value(fallback).unwrap(),
            Some(machine.intrinsics.object_prototype)
        );
    }

    #[test]
    fn bound_object_constructor_forwards_ordinary_and_distinct_new_target() {
        // B2 -> B1 -> Object: ordinary construction preserves args; distinct NT ignores them.
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object_constructor = machine.intrinsics.global("Object").unwrap();
        let index = machine.runtime_slot(object_constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(object_id),
            ..
        } = machine.heap[index]
        else {
            panic!("Object is a builtin");
        };
        let b1 = call_method(
            &mut machine,
            object_constructor,
            "bind",
            &[Value::UNDEFINED],
        )
        .unwrap();
        let b2 = call_method(&mut machine, b1, "bind", &[Value::UNDEFINED]).unwrap();

        let existing = ordinary_object(&mut machine);
        machine
            .set_data_property(existing, "own", Value::int32(7))
            .unwrap();

        // Ordinary Construct(B2, [existing]) keeps Object's argument-return path.
        let returned = machine.construct_value(b2, &[existing]).unwrap();
        assert_eq!(returned, existing);

        // Ordinary Construct(B2, [99]) boxes the primitive.
        let boxed = machine.construct_value(b2, &[Value::int32(99)]).unwrap();
        assert!(machine.is_object(boxed));
        let boxed_slot = machine.runtime_slot(boxed).unwrap().unwrap();
        assert!(matches!(
            &machine.heap[boxed_slot],
            HeapEntry::Object {
                boxed_primitive: Some(_),
                ..
            }
        ));

        let custom_prototype = ordinary_object(&mut machine);
        machine
            .set_data_property(custom_prototype, "marker", Value::int32(42))
            .unwrap();
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: custom_prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        let unrelated = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();

        // Distinct unrelated NT through the bound chain ignores object/primitive args.
        let flat_obj = machine
            .flatten_bound(b2, Value::UNDEFINED, &[existing], unrelated)
            .unwrap();
        assert_eq!(flat_obj.target, object_constructor);
        assert_eq!(flat_obj.new_target, unrelated);
        let BuiltinOutcome::Value(fresh_obj) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &flat_obj.arguments,
                true,
                flat_obj.new_target,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_ne!(fresh_obj, existing);
        assert_eq!(
            machine.prototype_value(fresh_obj).unwrap(),
            Some(custom_prototype)
        );

        let flat_prim = machine
            .flatten_bound(b2, Value::UNDEFINED, &[Value::int32(99)], unrelated)
            .unwrap();
        let BuiltinOutcome::Value(fresh_prim) = machine
            .call_builtin_with_new_target(
                object_id,
                Value::UNDEFINED,
                &flat_prim.arguments,
                true,
                flat_prim.new_target,
            )
            .unwrap()
        else {
            panic!("Object construct returns a value");
        };
        assert_ne!(fresh_prim, boxed);
        assert_eq!(
            machine.prototype_value(fresh_prim).unwrap(),
            Some(custom_prototype)
        );
        let fresh_slot = machine.runtime_slot(fresh_prim).unwrap().unwrap();
        assert!(matches!(
            &machine.heap[fresh_slot],
            HeapEntry::Object {
                boxed_primitive: None,
                ..
            }
        ));
    }

    #[test]
    fn function_prototype_methods_reject_invalid_receivers_and_construction() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let invalid = ordinary_object(&mut machine);

        for name in ["call", "apply", "bind"] {
            let method = machine
                .get_named_property(machine.intrinsics.function_prototype, name)
                .unwrap();
            assert!(matches!(
                machine.call_value(method, invalid, &[]),
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ));
            let index = machine.runtime_slot(method).unwrap().unwrap();
            let HeapEntry::NativeFunction {
                callable: NativeCallable::Builtin(id),
                ..
            } = machine.heap[index]
            else {
                panic!("Function.prototype method is a builtin");
            };
            assert!(matches!(
                machine.call_builtin(id, target, &[], true),
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ));
        }
    }

    #[test]
    fn bound_function_reports_callable_identity() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let bound = call_method(&mut machine, target, "bind", &[]).unwrap();

        assert_eq!(machine.type_of(bound), "function");
        let tag = machine
            .call_value(machine.intrinsics.object_to_string(), bound, &[])
            .unwrap();
        assert!(
            machine
                .string_value(tag)
                .is_some_and(|text| text.eq_ascii("[object Function]"))
        );
        assert_eq!(
            machine.prototype_value(bound).unwrap(),
            Some(machine.intrinsics.function_prototype)
        );
        assert!(
            machine
                .to_string(bound)
                .is_ok_and(|text| text.eq_ascii("function () { [native code] }"))
        );
    }

    #[test]
    fn function_prototype_methods_are_ordinary_own_properties() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let call_key = PropertyKey::Named(EcmaString::encode("call"));

        assert!(!machine.has_own_property_key(target, &call_key).unwrap());
        machine
            .set_data_property_key(target, call_key.clone(), Value::int32(1))
            .unwrap();
        assert_eq!(
            machine.get_property_key(target, &call_key).unwrap(),
            Value::int32(1)
        );
        for name in ["call", "apply", "bind"] {
            let method = machine
                .get_named_property(machine.intrinsics.function_prototype, name)
                .unwrap();
            assert!(machine.is_callable(method).unwrap());
        }
    }

    #[test]
    fn bound_and_applied_argument_lists_respect_the_limit() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(
            &module,
            &mut host,
            Limits {
                max_argument_count: 4,
                ..Limits::default()
            },
        );
        let target = probe(&mut machine, "probe", 2);
        let receiver = ordinary_object(&mut machine);
        let bound = call_method(
            &mut machine,
            target,
            "bind",
            &[
                receiver,
                Value::int32(1),
                Value::int32(2),
                Value::int32(3),
                Value::int32(4),
            ],
        )
        .unwrap();
        assert!(matches!(
            machine.call_value(bound, Value::UNDEFINED, &[Value::int32(5)]),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::ArgumentLimitExceeded {
                    limit: 4,
                    requested: 5,
                }
            ))
        ));

        let arguments = allocate_array(&mut machine, vec![Value::UNDEFINED; 5]).unwrap();
        assert!(matches!(
            call_method(&mut machine, target, "apply", &[receiver, arguments]),
            Err(EvalFailure::Runtime(
                RuntimeErrorKind::ArgumentLimitExceeded {
                    limit: 4,
                    requested: 5,
                }
            ))
        ));
    }

    #[test]
    fn deep_bound_call_chains_use_constant_native_stack() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let call = machine
            .get_named_property(machine.intrinsics.function_prototype, "call")
            .unwrap();
        let mut head = target;
        for _ in 0..50_000 {
            head = machine
                .allocate(HeapEntry::native_function(
                    NativeCallable::Bound(Box::new(BoundCallable {
                        target: call,
                        this_value: head,
                        arguments: Vec::new(),
                    })),
                    PropertyMap::default(),
                    None,
                ))
                .unwrap();
        }
        let result = machine.call_value(head, Value::UNDEFINED, &[]).unwrap();
        assert_eq!(
            machine.array_elements(result).unwrap().unwrap(),
            vec![Value::UNDEFINED]
        );

        let receiver = ordinary_object(&mut machine);
        let mut arguments = Vec::with_capacity(machine.limits.max_argument_count as usize);
        arguments.push(target);
        arguments.push(receiver);
        arguments.resize(machine.limits.max_argument_count as usize, Value::UNDEFINED);
        let result = machine.call_value(call, call, &arguments).unwrap();
        let values = machine.array_elements(result).unwrap().unwrap();
        assert_eq!(values.len(), machine.limits.max_argument_count as usize - 1);
        assert_eq!(values[0], receiver);
    }

    // ---- from_entries iterable tests ---------------------------------------

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

    fn custom_iterator_return<H: Host>(
        machine: &mut Machine<'_, H>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine.set_data_property(this, "_closed", Value::TRUE)?;
        Ok(BuiltinOutcome::Value(this))
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
        let close = machine.get_named_property(this, "_return")?;
        machine.set_data_property(iter, "_values", values)?;
        machine.set_data_property(iter, "_index", Value::int32(0))?;
        machine.set_data_property(iter, "_closed", Value::FALSE)?;
        machine.set_data_property(iter, "next", next)?;
        machine.set_data_property(iter, "return", close)?;
        machine.test_set_global("fromEntriesIterator", iter);
        Ok(BuiltinOutcome::Value(iter))
    }

    fn custom_iterable(machine: &mut Machine<'_, TestHost>, values: Vec<Value>) -> Value {
        let next_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries next",
                length: 0,
                handler: custom_iterator_next::<TestHost>,
            });
        let next_fn =
            crate::intrinsics::native_function(&mut machine.heap, next_id, "from entries next", 0);
        let return_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries return",
                length: 0,
                handler: custom_iterator_return::<TestHost>,
            });
        let return_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            return_id,
            "from entries return",
            0,
        );
        let create_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from entries iterator",
                length: 0,
                handler: custom_iterator_create::<TestHost>,
            });
        let create_fn = crate::intrinsics::native_function(
            &mut machine.heap,
            create_id,
            "from entries iterator",
            0,
        );
        let iterable = ordinary_object(machine);
        let values_array = allocate_array(machine, values).unwrap();
        machine
            .set_data_property(iterable, "_values", values_array)
            .unwrap();
        machine
            .set_data_property(iterable, "_next", next_fn)
            .unwrap();
        machine
            .set_data_property(iterable, "_return", return_fn)
            .unwrap();
        let iterator_symbol = machine.intrinsics.builtins.symbol_iterator();
        let iterator_key = machine.to_property_key(iterator_symbol).unwrap();
        machine
            .set_data_property_key(iterable, iterator_key, create_fn)
            .unwrap();
        iterable
    }

    fn entry_pair(machine: &mut Machine<'_, TestHost>, key: &str, value: Value) -> Value {
        let entry = ordinary_object(machine);
        let key_str = allocate_string(machine, EcmaString::encode(key)).unwrap();
        machine.set_data_property(entry, "0", key_str).unwrap();
        machine.set_data_property(entry, "1", value).unwrap();
        entry
    }

    fn throwing_from_entries_value_getter<H: Host>(
        _machine: &mut Machine<'_, H>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Err(type_error("entry value getter failed"))
    }

    fn collecting_from_entries_key_getter<H: Host>(
        machine: &mut Machine<'_, H>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine.collect_garbage();
        Ok(BuiltinOutcome::Value(
            machine
                .test_global("fromEntriesKey")
                .expect("key remains globally rooted"),
        ))
    }

    fn collecting_from_entries_value_getter<H: Host>(
        machine: &mut Machine<'_, H>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine.test_set_global("fromEntriesKey", Value::UNDEFINED);
        machine.collect_garbage();
        Ok(BuiltinOutcome::Value(Value::int32(7)))
    }

    fn rejecting_inherited_setter<H: Host>(
        _machine: &mut Machine<'_, H>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Err(type_error("inherited setter called"))
    }

    fn assert_from_entries_closed_after_one_step(machine: &mut Machine<'_, TestHost>) {
        let iterator = machine
            .test_global("fromEntriesIterator")
            .expect("iterator is captured");
        assert_eq!(
            machine.get_named_property(iterator, "_index").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(iterator, "_closed").unwrap(),
            Value::TRUE
        );
    }

    #[test]
    fn from_entries_consumes_generic_iterable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let e1 = entry_pair(&mut machine, "a", Value::int32(1));
        let e2 = entry_pair(&mut machine, "b", Value::int32(2));
        let source = custom_iterable(&mut machine, vec![e1, e2]);

        let result = call_object(&mut machine, "fromEntries", &[source]).unwrap();
        assert_eq!(
            machine.get_named_property(result, "a").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(result, "b").unwrap(),
            Value::int32(2)
        );
    }

    #[test]
    fn from_entries_accepts_object_shaped_entries() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Entries are plain objects with "0"/"1" properties, not arrays.
        let e1 = entry_pair(&mut machine, "x", Value::int32(10));
        let e2 = entry_pair(&mut machine, "y", Value::int32(20));
        let source = custom_iterable(&mut machine, vec![e1, e2]);

        let result = call_object(&mut machine, "fromEntries", &[source]).unwrap();
        assert_eq!(
            machine.get_named_property(result, "x").unwrap(),
            Value::int32(10)
        );
        assert_eq!(
            machine.get_named_property(result, "y").unwrap(),
            Value::int32(20)
        );
    }

    #[test]
    fn from_entries_rejects_primitive_entries() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Iterator yields a primitive number, not an object-shaped entry.
        let source = custom_iterable(&mut machine, vec![Value::int32(42)]);
        let result = call_object(&mut machine, "fromEntries", &[source]);
        assert!(
            result.is_err(),
            "Object.fromEntries with primitive entry must fail"
        );
    }

    #[test]
    fn from_entries_closes_before_advancing_past_invalid_entry() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let second = entry_pair(&mut machine, "later", Value::int32(2));
        let source = custom_iterable(&mut machine, vec![Value::int32(42), second]);

        assert!(matches!(
            call_object(&mut machine, "fromEntries", &[source]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert_from_entries_closed_after_one_step(&mut machine);
    }

    #[test]
    fn from_entries_closes_before_advancing_after_value_getter_failure() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let first = ordinary_object(&mut machine);
        let key = allocate_string(&mut machine, EcmaString::encode("first")).unwrap();
        machine.set_data_property(first, "0", key).unwrap();
        let getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries throwing value getter",
                length: 0,
                handler: throwing_from_entries_value_getter::<TestHost>,
            });
        let getter = crate::intrinsics::native_function(
            &mut machine.heap,
            getter_id,
            "from entries throwing value getter",
            0,
        );
        machine
            .define_descriptor(
                first,
                PropertyKey::Named(EcmaString::encode("1")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let second = entry_pair(&mut machine, "later", Value::int32(2));
        let source = custom_iterable(&mut machine, vec![first, second]);

        assert!(matches!(
            call_object(&mut machine, "fromEntries", &[source]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert_from_entries_closed_after_one_step(&mut machine);
    }

    #[test]
    fn from_entries_roots_result_across_entry_getters() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let entry = ordinary_object(&mut machine);
        let key = symbol(&mut machine, "rooted");
        machine.test_set_global("fromEntriesKey", key);
        let key_getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries collecting key getter",
                length: 0,
                handler: collecting_from_entries_key_getter::<TestHost>,
            });
        let key_getter = crate::intrinsics::native_function(
            &mut machine.heap,
            key_getter_id,
            "from entries collecting key getter",
            0,
        );
        machine
            .define_descriptor(
                entry,
                PropertyKey::Named(EcmaString::encode("0")),
                Property::Accessor {
                    getter: Some(key_getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let getter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries collecting value getter",
                length: 0,
                handler: collecting_from_entries_value_getter::<TestHost>,
            });
        let getter = crate::intrinsics::native_function(
            &mut machine.heap,
            getter_id,
            "from entries collecting value getter",
            0,
        );
        machine
            .define_descriptor(
                entry,
                PropertyKey::Named(EcmaString::encode("1")),
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        let source = custom_iterable(&mut machine, vec![entry]);

        let result = call_object(&mut machine, "fromEntries", &[source]).unwrap();
        assert_eq!(
            machine
                .get_property_key(result, &symbol_key(&machine, key))
                .unwrap(),
            Value::int32(7)
        );
        let description = machine.get_named_property(key, "description").unwrap();
        assert!(
            machine
                .string_value(description)
                .is_some_and(|text| text.eq_ascii("rooted"))
        );
    }

    #[test]
    fn from_entries_ignores_inherited_setters() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let setter_id = machine
            .intrinsics
            .builtins
            .register(crate::intrinsics::BuiltinDef {
                name: "from_entries inherited setter",
                length: 1,
                handler: rejecting_inherited_setter::<TestHost>,
            });
        let setter = crate::intrinsics::native_function(
            &mut machine.heap,
            setter_id,
            "from entries inherited setter",
            1,
        );
        machine
            .define_descriptor(
                machine.intrinsics.object_prototype,
                PropertyKey::Named(EcmaString::encode("own")),
                Property::Accessor {
                    getter: None,
                    setter: Some(setter),
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        let entry = entry_pair(&mut machine, "own", Value::int32(9));
        let source = custom_iterable(&mut machine, vec![entry]);

        let result = call_object(&mut machine, "fromEntries", &[source]).unwrap();
        assert!(matches!(
            machine
                .own_descriptor(result, &PropertyKey::Named(EcmaString::encode("own")))
                .unwrap(),
            Some(Property::Data {
                value,
                writable: true,
                enumerable: true,
                configurable: true,
            }) if value == Value::int32(9)
        ));
    }

    #[test]
    fn from_entries_consumes_array_through_protocol() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let a_key = allocate_string(&mut machine, EcmaString::encode("a")).unwrap();
        let e1 = allocate_array(&mut machine, vec![a_key, Value::int32(1)]).unwrap();
        let b_key = allocate_string(&mut machine, EcmaString::encode("b")).unwrap();
        let e2 = allocate_array(&mut machine, vec![b_key, Value::int32(2)]).unwrap();
        let source = allocate_array(&mut machine, vec![e1, e2]).unwrap();

        let result = call_object(&mut machine, "fromEntries", &[source]).unwrap();
        assert_eq!(
            machine.get_named_property(result, "a").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(result, "b").unwrap(),
            Value::int32(2)
        );
    }

    // ---- Finding 1: ValidateAndApplyPropertyDescriptor ---------------------

    fn non_configurable_data(machine: &mut Machine<'_, TestHost>) -> Value {
        let target = ordinary_object(machine);
        let descriptor = ordinary_object(machine);
        machine
            .set_data_property(descriptor, "value", Value::int32(1))
            .unwrap();
        machine
            .set_data_property(descriptor, "writable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(descriptor, "configurable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(descriptor, "enumerable", Value::FALSE)
            .unwrap();
        let key = allocate_string(machine, EcmaString::encode("x")).unwrap();
        call_object(machine, "defineProperty", &[target, key, descriptor]).unwrap();
        target
    }

    #[test]
    fn redefining_non_configurable_value_throws() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = non_configurable_data(&mut machine);
        let change_value = ordinary_object(&mut machine);
        machine
            .set_data_property(change_value, "value", Value::int32(2))
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        let result = call_object(&mut machine, "defineProperty", &[target, key, change_value]);
        assert!(
            matches!(
                result,
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "redefining value of non-writable non-configurable property must throw TypeError"
        );
        assert_eq!(
            machine.get_named_property(target, "x").unwrap(),
            Value::int32(1),
            "value must be unchanged after rejected redefinition"
        );
    }

    #[test]
    fn redefining_non_configurable_to_configurable_throws() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = non_configurable_data(&mut machine);
        let make_configurable = ordinary_object(&mut machine);
        machine
            .set_data_property(make_configurable, "configurable", Value::TRUE)
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        let result = call_object(
            &mut machine,
            "defineProperty",
            &[target, key, make_configurable],
        );
        assert!(
            matches!(
                result,
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "making a non-configurable property configurable must throw TypeError"
        );
    }

    #[test]
    fn redefining_non_configurable_to_accessor_throws() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = non_configurable_data(&mut machine);
        let to_accessor = ordinary_object(&mut machine);
        let getter = probe(&mut machine, "getter", 0);
        machine
            .set_data_property(to_accessor, "get", getter)
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        let result = call_object(&mut machine, "defineProperty", &[target, key, to_accessor]);
        assert!(
            matches!(
                result,
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "converting non-configurable data property to accessor must throw TypeError"
        );
    }

    #[test]
    fn freeze_then_define_property_throws() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        machine
            .set_data_property(target, "x", Value::int32(1))
            .unwrap();
        call_object(&mut machine, "freeze", &[target]).unwrap();
        let change_value = ordinary_object(&mut machine);
        machine
            .set_data_property(change_value, "value", Value::int32(4))
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        let result = call_object(&mut machine, "defineProperty", &[target, key, change_value]);
        assert!(
            matches!(
                result,
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "defineProperty on a frozen property must throw TypeError"
        );
        assert_eq!(
            machine.get_named_property(target, "x").unwrap(),
            Value::int32(1),
            "frozen value must be unchanged"
        );
    }

    #[test]
    fn legal_no_op_redefinition_of_non_configurable_succeeds() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = non_configurable_data(&mut machine);
        // Redefine with the exact same values — spec permits this.
        let no_op = ordinary_object(&mut machine);
        machine
            .set_data_property(no_op, "value", Value::int32(1))
            .unwrap();
        machine
            .set_data_property(no_op, "writable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(no_op, "configurable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(no_op, "enumerable", Value::FALSE)
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        call_object(&mut machine, "defineProperty", &[target, key, no_op])
            .expect("no-op redefinition of non-configurable property must succeed");
        assert_eq!(
            machine.get_named_property(target, "x").unwrap(),
            Value::int32(1)
        );
    }

    #[test]
    fn making_non_writable_non_configurable_writable_throws() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = non_configurable_data(&mut machine);
        let make_writable = ordinary_object(&mut machine);
        machine
            .set_data_property(make_writable, "writable", Value::TRUE)
            .unwrap();
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();
        let result = call_object(
            &mut machine,
            "defineProperty",
            &[target, key, make_writable],
        );
        assert!(
            matches!(
                result,
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "making a non-writable non-configurable property writable must throw TypeError"
        );
    }

    // ---- Finding 2: bound function inherits target's prototype -------------

    #[test]
    fn bound_function_inherits_target_prototype() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Create a user function with a custom [[Prototype]]. The custom
        // prototype must itself inherit from %Function.prototype% so that
        // `bind` (which lives on %Function.prototype%) stays reachable on
        // the target — exactly as `Object.setPrototypeOf(fn, customProto)`
        // would in real JS only if customProto's chain reaches
        // %Function.prototype%. A plain ordinary object here would sever
        // that chain and make `target.bind` resolve to `undefined`.
        let custom_prototype = ordinary_object(&mut machine);
        machine
            .set_prototype_value(
                custom_prototype,
                Some(machine.intrinsics.function_prototype),
            )
            .unwrap();
        let target = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties: PropertyMap::default(),
                prototype: Some(custom_prototype),
                extensible: true,
            })
            .unwrap();

        let bound = call_method(&mut machine, target, "bind", &[Value::UNDEFINED]).unwrap();

        // The bound function must inherit the target's prototype, not the
        // default %Function.prototype%.
        assert_eq!(
            machine.prototype_value(bound).unwrap(),
            Some(custom_prototype),
            "bound function must inherit target's prototype"
        );
    }

    #[test]
    fn bound_builtin_still_gets_function_prototype() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = probe(&mut machine, "probe", 2);
        let bound = call_method(&mut machine, target, "bind", &[]).unwrap();
        // A builtin has prototype: None, which falls back to %Function.prototype%.
        assert_eq!(
            machine.prototype_value(bound).unwrap(),
            Some(machine.intrinsics.function_prototype),
            "bound builtin must still get %Function.prototype%"
        );
    }

    #[test]
    fn same_value_handles_nan_and_signed_zero_across_number_and_int32() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());

        let nan = Value::number(f64::NAN);
        let pos_zero = Value::number(0.0);
        let neg_zero = Value::number(-0.0);
        let int_zero = Value::int32(0);
        let int_five = Value::int32(5);

        // Number/Number: NaN equals NaN, and +0/-0 stay distinct.
        assert!(
            same_value(&machine, nan, nan),
            "NaN === NaN under SameValue"
        );
        assert!(
            !same_value(&machine, nan, pos_zero),
            "NaN !== 0 under SameValue"
        );
        assert!(
            !same_value(&machine, pos_zero, neg_zero),
            "+0 !== -0 under SameValue"
        );
        assert!(
            same_value(&machine, neg_zero, neg_zero),
            "-0 === -0 under SameValue"
        );
        assert!(same_value(&machine, pos_zero, pos_zero), "+0 === +0");

        // Number/Int32: the Int32 side is always finite, so NaN can only come
        // from the Number side and must NOT match any integer. Int32(0) is
        // +0.0, so -0.0 must be distinct from it while +0.0 matches.
        assert!(
            !same_value(&machine, nan, int_zero),
            "NaN !== Int32(0) under SameValue"
        );
        assert!(
            !same_value(&machine, int_zero, nan),
            "Int32(0) !== NaN under SameValue (symmetric)"
        );
        assert!(
            same_value(&machine, pos_zero, int_zero),
            "+0.0 === Int32(0)"
        );
        assert!(
            !same_value(&machine, neg_zero, int_zero),
            "-0.0 !== Int32(0): signed-zero distinction crosses the mixed branch"
        );
        assert!(
            same_value(&machine, Value::number(5.0), int_five),
            "5.0 === Int32(5)"
        );
    }

    #[test]
    fn same_value_negative_int32_matches_equivalent_number() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());

        let neg_one_number = Value::number(-1.0);
        let neg_one_int32 = Value::int32((-1i32) as u32);

        assert!(
            same_value(&machine, neg_one_number, neg_one_int32),
            "-1.0 === Int32(-1) under SameValue"
        );
        assert!(
            same_value(&machine, neg_one_int32, neg_one_number),
            "Int32(-1) === -1.0 under SameValue (symmetric)"
        );

        // Representation independence: the raw u32 payload 0xFFFF_FFFF must
        // not be confused with the positive Number 4_294_967_295.0.
        let large_unsigned = Value::number(4_294_967_295.0);
        assert!(
            !same_value(&machine, large_unsigned, neg_one_int32),
            "4_294_967_295.0 !== Int32(-1)"
        );

        // NaN and signed zero must remain untouched.
        assert!(
            !same_value(&machine, Value::number(f64::NAN), neg_one_int32),
            "NaN !== Int32(-1) under SameValue"
        );
        assert!(
            !same_value(&machine, Value::number(-0.0), Value::int32(0)),
            "-0.0 !== Int32(0) under SameValue"
        );
    }

    #[test]
    fn redefining_non_writable_property_across_representations_succeeds() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let target = ordinary_object(&mut machine);
        let key = allocate_string(&mut machine, EcmaString::encode("x")).unwrap();

        // Initial value is an Int32 -1 (0xFFFF_FFFF payload).
        let initial = ordinary_object(&mut machine);
        machine
            .set_data_property(initial, "value", Value::int32((-1i32) as u32))
            .unwrap();
        machine
            .set_data_property(initial, "writable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(initial, "configurable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(initial, "enumerable", Value::FALSE)
            .unwrap();
        call_object(&mut machine, "defineProperty", &[target, key, initial]).unwrap();

        // Redefine with the same mathematical value as a Number -1.0.
        let redefinition = ordinary_object(&mut machine);
        machine
            .set_data_property(redefinition, "value", Value::number(-1.0))
            .unwrap();
        machine
            .set_data_property(redefinition, "writable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(redefinition, "configurable", Value::FALSE)
            .unwrap();
        machine
            .set_data_property(redefinition, "enumerable", Value::FALSE)
            .unwrap();

        call_object(&mut machine, "defineProperty", &[target, key, redefinition])
            .expect("redefinition with representation-independent SameValue must succeed");

        assert_eq!(
            machine.get_named_property(target, "x").unwrap(),
            Value::number(-1.0),
            "property value must use the new Number representation"
        );
    }
}
