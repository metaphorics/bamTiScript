use std::collections::BTreeMap;

use bamts_bytecode::{DescriptorSlot, EcmaString};
use bamts_native::{Decoded, Value};

use super::{BuiltinDef, BuiltinOutcome, BuiltinTable, native_function, push};
use crate::{
    EvalFailure, GetOutcome, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap,
    SetOutcome, ThrowOrigin,
};

mod array;
mod array_es2023;
pub(crate) mod arraybuffer;
mod atomics;
pub(crate) mod bigint;
mod collections;
pub(crate) mod dataview;
mod date_full;
mod error_edge;
mod json_edge;
mod map_set_edge;
mod math_edge;
mod number;
mod number_format;
mod object;
mod object_statics;
mod promise;
mod property_descriptor;
pub(crate) mod proxy;
mod reflect;
mod regexp;
pub(crate) mod regexp_v;
mod set_methods;
pub(crate) use regexp::canonical_source;
mod annex_b;
mod string;
mod string_edge;
mod structured_clone;
mod symbol;
#[cfg(test)]
mod test_support;
mod timers;
pub(crate) mod typedarray_all;
mod uri;
pub(crate) mod weakref_finalization;

pub(crate) use collections::ordinary_runtime;
pub(crate) use json_edge::evaluate_json_module_source;
pub(crate) use property_descriptor::PropertyDescriptor;
use typedarray_all::{read_element, typed_array_bounds, typed_array_index, write_element};

pub(crate) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
    timers_available: bool,
) {
    symbol::install(heap, globals, builtins);
    let bigint_prototype = bigint::install(heap, globals, builtins);
    builtins.set_bigint_prototype(bigint_prototype);
    promise::install(heap, globals, builtins);
    if timers_available {
        timers::install(heap, globals, builtins);
    }
    collections::install_iterator_prototype(heap, builtins);
    collections::install_async_iterator_prototype(heap, builtins);
    collections::install_generator_prototype(heap, builtins);
    collections::install_async_generator_prototype(heap, builtins);
    collections::install_disposable_stacks(heap, globals, builtins);
    collections::install(heap, globals, builtins);
    map_set_edge::install(heap, globals, builtins);
    date_full::install(heap, globals, builtins);
    object::install(heap, globals, builtins);
    array::install(heap, globals, builtins);
    string::install(heap, globals, builtins);
    string_edge::install(heap, globals, builtins);
    number::install(heap, globals, builtins);
    install_boolean(heap, globals, builtins);
    install_math(heap, globals, builtins);
    regexp::install(heap, globals, builtins);
    arraybuffer::install(heap, globals, builtins);
    typedarray_all::install(heap, globals, builtins);
    dataview::install(heap, globals, builtins);
    atomics::install(heap, globals, builtins);
    weakref_finalization::install(heap, globals, builtins);
    install_globals(heap, globals, builtins);
    uri::install(heap, globals, builtins);
    json_edge::install(heap, globals, builtins);
    proxy::install(heap, globals, builtins);
    reflect::install(heap, globals, builtins);
    install_errors(heap, globals, builtins);
    error_edge::install(heap, globals, builtins);
    annex_b::install(heap, globals, builtins);
}

fn install_function<H: Host>(
    heap: &mut Vec<HeapEntry>,
    builtins: &mut BuiltinTable<H>,
    name: &'static str,
    length: u32,
    handler: super::BuiltinHandler<H>,
) -> Value {
    let id = builtins.register(BuiltinDef {
        name,
        length,
        handler,
    });
    let definition = builtins.get(id);
    native_function(heap, id, definition.name, definition.length)
}

pub(crate) fn define_data(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let index = heap_index(object);
    match &mut heap[index] {
        HeapEntry::Object { properties, .. }
        | HeapEntry::Array { properties, .. }
        | HeapEntry::Function { properties, .. }
        | HeapEntry::NativeFunction { properties, .. }
        | HeapEntry::ProxyRevoker { properties, .. }
        | HeapEntry::Script { properties, .. }
        | HeapEntry::RegExp { properties, .. }
        | HeapEntry::Date { properties, .. } => {
            properties.insert(
                PropertyKey::Named(EcmaString::encode(name)),
                builtin_property(value),
            );
        }
        _ => panic!("intrinsic property target must be an ordinary object"),
    }
}

pub(super) fn define_to_string_tag(
    heap: &mut Vec<HeapEntry>,
    object: Value,
    symbol: Value,
    tag: &str,
) {
    let value = push(heap, HeapEntry::String(EcmaString::encode(tag)));
    let index = heap_index(object);
    let HeapEntry::Object { properties, .. } = &mut heap[index] else {
        panic!("namespace tag target must be an ordinary object");
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(symbol) as u32),
        Property::Data {
            value,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
}

/// Installs a named data property that is non-writable, non-enumerable, and
/// non-configurable onto an ordinary object, array, or native function.
pub(crate) fn define_frozen_data(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let index = heap_index(object);
    let properties = match &mut heap[index] {
        HeapEntry::Object { properties, .. }
        | HeapEntry::Array { properties, .. }
        | HeapEntry::Function { properties, .. }
        | HeapEntry::Script { properties, .. }
        | HeapEntry::RegExp { properties, .. }
        | HeapEntry::NativeFunction { properties, .. } => properties,
        _ => panic!("frozen property target must be an ordinary object"),
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode(name)),
        Property::Data {
            value,
            writable: false,
            enumerable: false,
            configurable: false,
        },
    );
}

fn builtin_property(value: Value) -> Property {
    Property::Data {
        value,
        writable: true,
        enumerable: false,
        configurable: true,
    }
}

fn heap_index(value: Value) -> usize {
    let bamts_native::Decoded::HeapRef(id) = value.decode().expect("intrinsic value is valid")
    else {
        panic!("intrinsic value is a heap reference");
    };
    id.slot() as usize - 1
}

fn allocate_string<H: Host>(
    machine: &mut Machine<'_, H>,
    text: EcmaString,
) -> Result<Value, EvalFailure> {
    machine
        .allocate(HeapEntry::String(text))
        .map_err(EvalFailure::Runtime)
}

fn allocate_array<H: Host>(
    machine: &mut Machine<'_, H>,
    elements: Vec<Value>,
) -> Result<Value, EvalFailure> {
    machine
        .allocate(HeapEntry::Array {
            elements,
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.array_prototype),
            extensible: true,
            length_writable: true,
        })
        .map_err(EvalFailure::Runtime)
}

fn value_number(value: Value) -> f64 {
    match value.decode() {
        Some(bamts_native::Decoded::Int32(value)) => f64::from(value as i32),
        Some(bamts_native::Decoded::Number(value)) => value,
        _ => f64::NAN,
    }
}

fn to_integer_or_infinity<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Result<f64, EvalFailure> {
    let number = value_number(machine.to_number(value)?);
    Ok(if number.is_nan() || number == 0.0 {
        0.0
    } else {
        number.trunc()
    })
}

fn type_error(operation: &'static str) -> EvalFailure {
    EvalFailure::Throw(ThrowOrigin::TypeError { operation })
}

fn range_error(operation: &'static str) -> EvalFailure {
    EvalFailure::Throw(ThrowOrigin::RangeError { operation })
}

fn uri_error(operation: &'static str) -> EvalFailure {
    EvalFailure::Throw(ThrowOrigin::UriError { operation })
}

fn define_array_length(
    elements: &mut Vec<Value>,
    properties: &mut PropertyMap,
    length_writable: bool,
    length: usize,
) -> Result<(), EvalFailure> {
    if !length_writable && length != elements.len() {
        return Err(type_error("Cannot redefine non-writable array length"));
    }
    crate::apply_array_length(elements, properties, length, "define array length")
}

fn install_boolean<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.boolean_prototype();
    let constructor = install_function(heap, builtins, "Boolean", 1, boolean_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::encode("Boolean"), constructor);
    let value_of = install_function(heap, builtins, "valueOf", 0, boolean_value_of::<H>);
    define_data(heap, prototype, "valueOf", value_of);
}

fn boolean_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value =
        Value::boolean(machine.to_boolean(args.first().copied().unwrap_or(Value::UNDEFINED)));
    if constructing {
        return machine.box_primitive(value).map(BuiltinOutcome::Value);
    }
    Ok(BuiltinOutcome::Value(value))
}

fn boolean_value_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = machine.unbox_primitive(this, "Boolean.prototype.valueOf")?;
    match value.decode() {
        Some(bamts_native::Decoded::Boolean(_)) => Ok(BuiltinOutcome::Value(value)),
        _ => Err(type_error(
            "Boolean.prototype.valueOf called on incompatible receiver",
        )),
    }
}

fn install_math<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let math = push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(builtins.object_prototype()),
            extensible: true,
            boxed_primitive: None,
        },
    );
    for (name, length, handler) in [
        ("abs", 1, math_abs::<H> as super::BuiltinHandler<H>),
        ("floor", 1, math_floor::<H>),
        ("ceil", 1, math_ceil::<H>),
        ("round", 1, math_round::<H>),
        ("pow", 2, math_pow::<H>),
        ("sqrt", 1, math_sqrt::<H>),
        ("random", 0, math_random::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, math, name, function);
    }
    math_edge::install(heap, builtins, math);
    define_to_string_tag(heap, math, builtins.symbol_to_string_tag(), "Math");
    globals.insert(EcmaString::encode("Math"), math);
}

macro_rules! unary_math {
    ($name:ident, $body:expr) => {
        fn $name<H: Host>(
            machine: &mut Machine<'_, H>,
            _this: Value,
            args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            let x =
                value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?)
                    as f64;
            Ok(BuiltinOutcome::Value(crate::number_value(($body)(x))))
        }
    };
}

unary_math!(math_abs, f64::abs);
unary_math!(math_floor, f64::floor);
unary_math!(math_ceil, f64::ceil);
unary_math!(math_sqrt, f64::sqrt);

fn math_round<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let x = value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let rounded = if (-0.5..0.0).contains(&x) {
        -0.0
    } else {
        (x + 0.5).floor()
    };
    Ok(BuiltinOutcome::Value(crate::number_value(rounded)))
}

fn math_pow<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let x = value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let y = value_number(machine.to_number(args.get(1).copied().unwrap_or(Value::UNDEFINED))?);
    Ok(BuiltinOutcome::Value(crate::number_value(x.powf(y))))
}

fn math_random<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(crate::number_value(
        machine.host.random(),
    )))
}

fn install_globals<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    for (name, length, handler) in [
        (
            "parseInt",
            2,
            number::parse_int::<H> as super::BuiltinHandler<H>,
        ),
        ("parseFloat", 1, number::parse_float::<H>),
        ("isNaN", 1, number::global_is_nan::<H>),
        ("isFinite", 1, number::global_is_finite::<H>),
        (
            "structuredClone",
            1,
            structured_clone::structured_clone::<H>,
        ),
    ] {
        let value = install_function(heap, builtins, name, length, handler);
        globals.insert(EcmaString::encode(name), value);
    }
    globals.insert(
        EcmaString::encode("Infinity"),
        crate::number_value(f64::INFINITY),
    );
    globals.insert(EcmaString::encode("NaN"), crate::number_value(f64::NAN));
    let atomics = push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(builtins.object_prototype()),
            extensible: true,
            boxed_primitive: None,
        },
    );
    define_to_string_tag(heap, atomics, builtins.symbol_to_string_tag(), "Atomics");
    globals.insert(EcmaString::encode("Atomics"), atomics);
}

fn install_errors<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let error_prototype = push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(builtins.object_prototype()),
            extensible: true,
            boxed_primitive: None,
        },
    );
    let error_to_string = install_function(heap, builtins, "toString", 0, error_to_string::<H>);
    define_data(heap, error_prototype, "toString", error_to_string);
    install_error_type(
        heap,
        globals,
        builtins,
        "Error",
        1,
        error_prototype,
        error_prototype,
    );
    for name in [
        "EvalError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "TypeError",
        "URIError",
        "AggregateError",
        "SuppressedError",
    ] {
        let prototype = push(
            heap,
            HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(error_prototype),
                extensible: true,
                boxed_primitive: None,
            },
        );
        let length = match name {
            "AggregateError" => 2,
            "SuppressedError" => 3,
            _ => 1,
        };
        install_error_type(
            heap,
            globals,
            builtins,
            name,
            length,
            prototype,
            error_prototype,
        );
    }
}

fn install_error_type<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
    name: &'static str,
    length: u32,
    prototype: Value,
    _error_prototype: Value,
) {
    let name_value = push(heap, HeapEntry::String(EcmaString::encode(name)));
    let empty = push(heap, HeapEntry::String(EcmaString::default()));
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode("name")),
        Property::Data {
            value: name_value,
            writable: true,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        PropertyKey::Named(EcmaString::encode("message")),
        Property::Data {
            value: empty,
            writable: true,
            enumerable: false,
            configurable: true,
        },
    );
    let constructor = install_function(heap, builtins, name, length, error_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    builtins.set_error_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::encode(name), constructor);
}

fn error_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let id = machine
        .current_builtin_id()
        .ok_or_else(|| type_error("invalid error constructor"))?;
    let name = machine.intrinsics.builtins.get(id).name;
    let default_prototype = machine.intrinsics.error_prototype(id);
    let new_target = machine.current_new_target();
    let prototype = if new_target != Value::UNDEFINED {
        machine
            .constructed_prototype(new_target)
            .unwrap_or(default_prototype)
    } else {
        default_prototype
    };
    let object = if machine.inherits_from_prototype(this, prototype)? {
        this
    } else {
        machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?
    };
    let (message_index, options_index) = match name {
        "AggregateError" => {
            let errors = args.first().copied().unwrap_or(Value::UNDEFINED);
            let values = machine.iterable_values(errors)?;
            let array = allocate_array(machine, values)?;
            machine.set_data_property(object, "errors", array)?;
            (1, 2)
        }
        "SuppressedError" => {
            for (property, value) in [
                ("error", args.first().copied().unwrap_or(Value::UNDEFINED)),
                (
                    "suppressed",
                    args.get(1).copied().unwrap_or(Value::UNDEFINED),
                ),
            ] {
                machine.define_descriptor(
                    object,
                    PropertyKey::Named(EcmaString::encode(property)),
                    Property::Data {
                        value,
                        writable: true,
                        enumerable: false,
                        configurable: true,
                    },
                )?;
            }
            (2, 3)
        }
        _ => (0, 1),
    };
    let message = args
        .get(message_index)
        .filter(|value| **value != Value::UNDEFINED)
        .map(|value| machine.to_string(*value))
        .transpose()?;
    if let Some(message) = &message {
        let text = allocate_string(machine, message.clone())?;
        machine.set_data_property(object, "message", text)?;
    }
    if let Some(options) = args
        .get(options_index)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        let cause_key = PropertyKey::Named(EcmaString::encode("cause"));
        if machine.internal_has_property(options, &cause_key)? {
            let cause = machine.get_named_property(options, "cause")?;
            machine.set_data_property(object, "cause", cause)?;
        }
    }
    let mut stack = bamts_bytecode::EcmaStringBuilder::new();
    stack.push_utf8(name);
    if let Some(message) = &message
        && !message.is_empty()
    {
        stack.push_utf8(": ");
        for &unit in message.as_units() {
            stack.push_unit(unit);
        }
    }
    stack.push_utf8("\n    at <bamts>");
    let stack = allocate_string(machine, stack.finish())?;
    machine.set_data_property(object, "stack", stack)?;
    Ok(BuiltinOutcome::Value(object))
}

fn error_to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let name_value = machine.get_named_property(this, "name")?;
    let name = machine.to_string(name_value)?;
    let message_value = machine.get_named_property(this, "message")?;
    let message = machine.to_string(message_value)?;
    let text = if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        let mut text = bamts_bytecode::EcmaStringBuilder::with_capacity(
            name.len_units()
                .saturating_add(message.len_units())
                .saturating_add(2),
        );
        for &unit in name.as_units() {
            text.push_unit(unit);
        }
        text.push_utf8(": ");
        for &unit in message.as_units() {
            text.push_unit(unit);
        }
        text.finish()
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}

impl<'a, H: Host> Machine<'a, H> {
    pub(crate) fn internal_get(
        &mut self,
        object: Value,
        key: &PropertyKey,
        receiver: Value,
    ) -> Result<Value, EvalFailure> {
        if !matches!(key, PropertyKey::Private(_))
            && self
                .runtime_slot(object)
                .map_err(EvalFailure::Runtime)?
                .is_some_and(|index| matches!(self.heap[index], HeapEntry::Proxy { .. }))
        {
            return proxy::get(self, object, key, receiver);
        }
        match self.resolve_get(object, key, receiver)? {
            GetOutcome::Value(value) => Ok(value),
            GetOutcome::Text(text) => self
                .allocate(HeapEntry::String(text))
                .map_err(EvalFailure::Runtime),
            GetOutcome::Getter(getter) => self.call_value(getter, receiver, &[]),
        }
    }

    pub(crate) fn internal_set(
        &mut self,
        object: Value,
        key: PropertyKey,
        value: Value,
        receiver: Value,
    ) -> Result<bool, EvalFailure> {
        if !matches!(key, PropertyKey::Private(_))
            && self
                .runtime_slot(object)
                .map_err(EvalFailure::Runtime)?
                .is_some_and(|index| matches!(self.heap[index], HeapEntry::Proxy { .. }))
        {
            return proxy::set(self, object, key, value, receiver);
        }
        match self.resolve_set(object, key, value, receiver)? {
            SetOutcome::Done => Ok(true),
            SetOutcome::Failed => Ok(false),
            SetOutcome::Setter(setter) => {
                self.call_value(setter, receiver, &[value])?;
                Ok(true)
            }
        }
    }

    pub(crate) fn internal_get_own_property(
        &mut self,
        object: Value,
        key: &PropertyKey,
    ) -> Result<Option<PropertyDescriptor>, EvalFailure> {
        if self
            .runtime_slot(object)
            .map_err(EvalFailure::Runtime)?
            .is_some_and(|index| matches!(self.heap[index], HeapEntry::Proxy { .. }))
        {
            return proxy::get_own_property(self, object, key);
        }
        Ok(self
            .own_descriptor(object, key)?
            .map(property_descriptor::descriptor_from_property))
    }

    pub(crate) fn internal_define_own_property(
        &mut self,
        object: Value,
        key: PropertyKey,
        descriptor: PropertyDescriptor,
    ) -> Result<bool, EvalFailure> {
        if self
            .runtime_slot(object)
            .map_err(EvalFailure::Runtime)?
            .is_some_and(|index| matches!(self.heap[index], HeapEntry::Proxy { .. }))
        {
            return proxy::define_own_property(self, object, key, descriptor);
        }
        let current = self.own_descriptor(object, &key)?;
        let extensible = self.internal_is_extensible(object)?;
        property_descriptor::validate_and_apply_property_descriptor(
            self,
            Some((object, key)),
            extensible,
            descriptor,
            current,
        )
    }

    pub(crate) fn internal_get_prototype_of(
        &mut self,
        object: Value,
    ) -> Result<Option<Value>, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
        if matches!(self.heap[index], HeapEntry::Proxy { .. }) {
            return proxy::get_prototype_of(self, object);
        }
        self.prototype_index(index)?.map_or(Ok(None), |prototype| {
            let slot = u32::try_from(prototype + 1).map_err(|_| {
                EvalFailure::Throw(ThrowOrigin::RangeError {
                    operation: "prototype heap slot exceeds u32",
                })
            })?;
            Ok(Some(Value::heap_ref(
                bamts_native::SlotId::from_parts(crate::RUNTIME_HEAP_SEGMENT, slot)
                    .expect("runtime prototype slot is nonzero"),
            )))
        })
    }

    pub(crate) fn internal_set_prototype_of(
        &mut self,
        object: Value,
        prototype: Option<Value>,
    ) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        if matches!(self.heap[index], HeapEntry::Proxy { .. }) {
            return proxy::set_prototype_of(self, object, prototype);
        }
        if self.internal_get_prototype_of(object)? == prototype {
            return Ok(true);
        }
        if !self.internal_is_extensible(object)? {
            return Ok(false);
        }
        let mut candidate = prototype;
        let mut traversed = 0;
        while let Some(value) = candidate {
            if value == object {
                return Ok(false);
            }
            candidate = self.internal_get_prototype_of(value)?;
            traversed += 1;
            if traversed > self.heap.len() {
                return Ok(false);
            }
        }
        match &mut self.heap[index] {
            HeapEntry::Object {
                prototype: slot, ..
            }
            | HeapEntry::Generator {
                prototype: slot, ..
            }
            | HeapEntry::AsyncGenerator {
                prototype: slot, ..
            }
            | HeapEntry::AsyncFromSync {
                prototype: slot, ..
            }
            | HeapEntry::DisposableStack {
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
            | HeapEntry::ArrayBuffer {
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
            | HeapEntry::DataView {
                prototype: slot, ..
            }
            | HeapEntry::TypedArray {
                prototype: slot, ..
            }
            | HeapEntry::SharedArrayBuffer {
                prototype: slot, ..
            }
            | HeapEntry::Promise {
                prototype: slot, ..
            }
            | HeapEntry::WeakRef {
                prototype: slot, ..
            }
            | HeapEntry::FinalizationRegistry {
                prototype: slot, ..
            }
            | HeapEntry::ProcessEnv {
                prototype: slot, ..
            }
            | HeapEntry::Timeout {
                prototype: slot, ..
            } => {
                *slot = prototype;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn internal_is_extensible(&mut self, object: Value) -> Result<bool, EvalFailure> {
        let slot = self.runtime_slot(object).map_err(EvalFailure::Runtime)?;
        if slot.is_some_and(|index| matches!(self.heap[index], HeapEntry::Proxy { .. })) {
            return proxy::is_extensible(self, object);
        }
        if let Some(index) = slot
            && let HeapEntry::ProxyRevoker { extensible, .. } = &self.heap[index]
        {
            return Ok(*extensible);
        }
        property_descriptor::is_extensible(self, object)
    }

    pub(crate) fn internal_prevent_extensions(
        &mut self,
        object: Value,
    ) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(false);
        };
        if matches!(self.heap[index], HeapEntry::Proxy { .. }) {
            return proxy::prevent_extensions(self, object);
        }
        let extensible = match &mut self.heap[index] {
            HeapEntry::Object { extensible, .. }
            | HeapEntry::Array { extensible, .. }
            | HeapEntry::Function { extensible, .. }
            | HeapEntry::Script { extensible, .. }
            | HeapEntry::NativeFunction { extensible, .. }
            | HeapEntry::RegExp { extensible, .. }
            | HeapEntry::Date { extensible, .. }
            | HeapEntry::BuiltinIterator { extensible, .. }
            | HeapEntry::Collection { extensible, .. }
            | HeapEntry::DataView { extensible, .. }
            | HeapEntry::TypedArray { extensible, .. }
            | HeapEntry::ArrayBuffer { extensible, .. }
            | HeapEntry::SharedArrayBuffer { extensible, .. }
            | HeapEntry::WeakRef { extensible, .. }
            | HeapEntry::FinalizationRegistry { extensible, .. }
            | HeapEntry::AsyncFromSync { extensible, .. }
            | HeapEntry::DisposableStack { extensible, .. }
            | HeapEntry::Generator { extensible, .. }
            | HeapEntry::AsyncGenerator { extensible, .. }
            | HeapEntry::Promise { extensible, .. }
            | HeapEntry::Timeout { extensible, .. }
            | HeapEntry::ProcessEnv { extensible, .. }
            | HeapEntry::ProxyRevoker { extensible, .. } => extensible,
            _ => return Ok(false),
        };
        *extensible = false;
        Ok(true)
    }

    fn set_prototype_value(
        &mut self,
        object: Value,
        prototype: Option<Value>,
    ) -> Result<(), EvalFailure> {
        if !self.internal_set_prototype_of(object, prototype)? {
            return Err(type_error("set prototype"));
        }
        Ok(())
    }

    fn call_truthy(
        &mut self,
        callee: Value,
        this_value: Value,
        arguments: &[Value],
    ) -> Result<bool, EvalFailure> {
        let result = self.call_value(callee, this_value, arguments)?;
        Ok(self.to_boolean(result))
    }

    pub(crate) fn own_descriptor(
        &mut self,
        object: Value,
        key: &PropertyKey,
    ) -> Result<Option<Property>, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
        if matches!(self.heap[index], HeapEntry::TypedArray { .. })
            && let PropertyKey::Named(name) = key
            && let Some(typed_index) = typed_array_index(name)
        {
            let Some(element_index) = typed_index else {
                return Ok(None);
            };
            let bounds = typed_array_bounds(self, object)?;
            if bounds.detached || bounds.out_of_bounds || element_index >= bounds.element_length {
                return Ok(None);
            }
            let value = read_element(self, object, element_index)?;
            return Ok(Some(Property::Data {
                value,
                writable: true,
                enumerable: true,
                configurable: false,
            }));
        }
        let properties = match &self.heap[index] {
            HeapEntry::ModuleNamespace { module } => {
                let PropertyKey::Named(name) = key else {
                    return Ok(None);
                };
                return self
                    .namespace_export(*module, name)
                    .map(|value| {
                        value.map(|value| Property::Data {
                            value,
                            writable: true,
                            enumerable: true,
                            configurable: false,
                        })
                    })
                    .map_err(EvalFailure::Runtime);
            }
            HeapEntry::Array {
                elements,
                properties,
                length_writable,
                ..
            } => {
                if matches!(key, PropertyKey::Named(name) if name.eq_ascii("length")) {
                    return Ok(Some(Property::Data {
                        value: crate::number_value(elements.len() as f64),
                        writable: *length_writable,
                        enumerable: false,
                        configurable: false,
                    }));
                }
                if let Some(property) = properties.get(key) {
                    return Ok(Some(property.clone()));
                }
                if let PropertyKey::Named(name) = key
                    && let Some(offset) = crate::array_index(name)
                    && let Some(value) = elements.get(offset as usize)
                    && *value != Value::HOLE
                {
                    return Ok(Some(Property::Data {
                        value: *value,
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    }));
                }
                return Ok(None);
            }
            HeapEntry::DataView { properties, .. } | HeapEntry::TypedArray { properties, .. } => {
                properties
            }
            HeapEntry::Object { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::ProxyRevoker { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::ArrayBuffer { properties, .. }
            | HeapEntry::SharedArrayBuffer { properties, .. }
            | HeapEntry::WeakRef { properties, .. }
            | HeapEntry::FinalizationRegistry { properties, .. }
            | HeapEntry::Timeout { properties, .. } => properties,
            _ => return Ok(None),
        };
        Ok(properties.get(key).cloned())
    }

    pub(crate) fn define_descriptor(
        &mut self,
        object: Value,
        key: PropertyKey,
        descriptor: Property,
    ) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Err(type_error("Object.defineProperty called on non-object"));
        };
        if matches!(self.heap[index], HeapEntry::TypedArray { .. })
            && let PropertyKey::Named(name) = &key
            && let Some(typed_index) = typed_array_index(name)
        {
            let configurable = match &descriptor {
                Property::Data { configurable, .. } | Property::Accessor { configurable, .. } => {
                    *configurable
                }
            };
            if configurable {
                return Err(type_error("Invalid typed array index descriptor"));
            }
            let enumerable = match &descriptor {
                Property::Data { enumerable, .. } | Property::Accessor { enumerable, .. } => {
                    *enumerable
                }
            };
            if !enumerable {
                return Err(type_error("Invalid typed array index descriptor"));
            }
            let Property::Data {
                value, writable, ..
            } = descriptor
            else {
                return Err(type_error("Invalid typed array index descriptor"));
            };
            if !writable {
                return Err(type_error("Invalid typed array index descriptor"));
            }
            let Some(element_index) = typed_index else {
                return Err(type_error("Invalid typed array index descriptor"));
            };
            let bounds = typed_array_bounds(self, object)?;
            if bounds.detached || bounds.out_of_bounds || element_index >= bounds.element_length {
                return Err(type_error("Invalid typed array index descriptor"));
            }
            if !write_element(self, object, element_index, value)? {
                return Err(type_error("Invalid typed array index descriptor"));
            }
            return Ok(());
        }
        if matches!(&key, PropertyKey::Named(name) if name.eq_ascii("length"))
            && matches!(self.heap[index], HeapEntry::Array { .. })
        {
            let Property::Data {
                value,
                writable,
                enumerable: false,
                configurable: false,
            } = descriptor
            else {
                return Err(type_error("Cannot redefine array length"));
            };
            let length = crate::exact_array_length(value)
                .ok_or_else(|| range_error("define array length"))?;
            let (old_length, old_property_bytes, length_writable) = match &self.heap[index] {
                HeapEntry::Array {
                    elements,
                    properties,
                    length_writable,
                    ..
                } => (elements.len(), properties.charge_bytes(), *length_writable),
                _ => unreachable!("array checked above"),
            };
            if writable && !length_writable {
                return Err(type_error("Cannot make array length writable"));
            }
            let growth = length
                .saturating_sub(old_length)
                .saturating_mul(std::mem::size_of::<Value>());
            self.charge_slot(index, growth)
                .map_err(EvalFailure::Runtime)?;
            let result = {
                let HeapEntry::Array {
                    elements,
                    properties,
                    length_writable,
                    ..
                } = &mut self.heap[index]
                else {
                    unreachable!("array checked above");
                };
                let result = define_array_length(elements, properties, *length_writable, length);
                if !writable {
                    *length_writable = false;
                }
                result
            };
            let (new_length, new_property_bytes) = match &self.heap[index] {
                HeapEntry::Array {
                    elements,
                    properties,
                    ..
                } => (elements.len(), properties.charge_bytes()),
                _ => unreachable!("array checked above"),
            };
            let released = old_length
                .saturating_sub(new_length)
                .saturating_mul(std::mem::size_of::<Value>())
                .saturating_add(old_property_bytes.saturating_sub(new_property_bytes));
            self.refund_slot(index, released);
            return result;
        }
        let (extensible, exists, property_growth, property_refund, element_growth, array_index) =
            match &self.heap[index] {
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
                | HeapEntry::Script {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::NativeFunction {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::ProxyRevoker {
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
                | HeapEntry::DataView {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::TypedArray {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::ArrayBuffer {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::SharedArrayBuffer {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::WeakRef {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::FinalizationRegistry {
                    properties,
                    extensible,
                    ..
                }
                | HeapEntry::Timeout {
                    properties,
                    extensible,
                    ..
                } => {
                    let exists = properties.contains_key(&key);
                    let (property_growth, property_refund) =
                        property_definition_charges(properties, &key, &descriptor);
                    (
                        *extensible,
                        exists,
                        property_growth,
                        property_refund,
                        0,
                        None,
                    )
                }
                HeapEntry::Array {
                    elements,
                    properties,
                    extensible,
                    length_writable,
                    ..
                } => {
                    let array_index = key
                        .as_string()
                        .and_then(crate::array_index)
                        .map(|offset| offset as usize);
                    let exists = properties.contains_key(&key)
                        || array_index.is_some_and(|offset| {
                            elements
                                .get(offset)
                                .is_some_and(|element| *element != Value::HOLE)
                        });
                    if !*extensible && !exists {
                        return Err(type_error(
                            "Cannot define property, object is not extensible",
                        ));
                    }
                    if let Some(offset) = array_index
                        && offset >= elements.len()
                        && !*length_writable
                    {
                        return Err(type_error(
                            "Cannot define index beyond non-writable array length",
                        ));
                    }
                    let element_growth = array_index.map_or(0, |offset| {
                        (offset + 1)
                            .saturating_sub(elements.len())
                            .saturating_mul(std::mem::size_of::<Value>())
                    });
                    let (property_growth, property_refund) =
                        property_definition_charges(properties, &key, &descriptor);
                    (
                        *extensible,
                        exists,
                        property_growth,
                        property_refund,
                        element_growth,
                        array_index,
                    )
                }
                _ => return Err(type_error("Object.defineProperty called on non-object")),
            };
        if !extensible && !exists {
            return Err(type_error(
                "Cannot define property, object is not extensible",
            ));
        }
        self.charge_slot(index, property_growth.saturating_add(element_growth))
            .map_err(EvalFailure::Runtime)?;
        match &mut self.heap[index] {
            HeapEntry::Array {
                elements,
                properties,
                length_writable,
                ..
            } => {
                if let Some(offset) = array_index {
                    if elements.len() <= offset {
                        crate::array_set_length(
                            elements,
                            properties,
                            *length_writable,
                            crate::number_value((offset + 1) as f64),
                            "define array index",
                        )?;
                    }
                    elements[offset] = Value::HOLE;
                }
                properties.insert(key, descriptor);
            }
            HeapEntry::Object { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::Script { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::ProxyRevoker { properties, .. }
            | HeapEntry::RegExp { properties, .. }
            | HeapEntry::Date { properties, .. }
            | HeapEntry::BuiltinIterator { properties, .. }
            | HeapEntry::Collection { properties, .. }
            | HeapEntry::DataView { properties, .. }
            | HeapEntry::TypedArray { properties, .. }
            | HeapEntry::ArrayBuffer { properties, .. }
            | HeapEntry::SharedArrayBuffer { properties, .. }
            | HeapEntry::WeakRef { properties, .. }
            | HeapEntry::FinalizationRegistry { properties, .. }
            | HeapEntry::Timeout { properties, .. } => {
                properties.insert(key, descriptor);
            }
            _ => unreachable!("validated object cannot change heap entry kind"),
        }
        self.refund_slot(index, property_refund);
        Ok(())
    }

    fn observable_property_key(&mut self, key: Value) -> Result<PropertyKey, EvalFailure> {
        let primitive = self.coerce_primitive_observable(key, true)?;
        Ok(
            match self.runtime_slot(primitive).map_err(EvalFailure::Runtime)? {
                Some(index) if matches!(self.heap[index], HeapEntry::Symbol { .. }) => {
                    PropertyKey::Symbol(index as u32)
                }
                Some(index) if matches!(self.heap[index], HeapEntry::PrivateName { .. }) => {
                    PropertyKey::Private(index as u32)
                }
                _ => PropertyKey::Named(self.to_string(primitive)?),
            },
        )
    }

    /// Read one own-descriptor slot without invoking accessors or walking the
    /// prototype chain. Absent properties, non-object primitives (other than
    /// `null`/`undefined`), and shape mismatches yield `undefined`.
    pub(crate) fn load_own_descriptor_slot(
        &mut self,
        object: Value,
        key: Value,
        slot: DescriptorSlot,
    ) -> Result<Value, EvalFailure> {
        if matches!(object.decode(), Some(Decoded::Undefined | Decoded::Null)) {
            return Err(type_error("Cannot convert undefined or null to object"));
        }
        let key = self.observable_property_key(key)?;
        let Some(property) = self.own_descriptor(object, &key)? else {
            return Ok(Value::UNDEFINED);
        };
        Ok(match (property, slot) {
            (Property::Data { value, .. }, DescriptorSlot::Value) => value,
            (Property::Accessor { getter, .. }, DescriptorSlot::Getter) => {
                getter.unwrap_or(Value::UNDEFINED)
            }
            (Property::Accessor { setter, .. }, DescriptorSlot::Setter) => {
                setter.unwrap_or(Value::UNDEFINED)
            }
            _ => Value::UNDEFINED,
        })
    }

    /// Write one own-descriptor slot, creating an absent property when needed
    /// and preserving attributes plus the opposite accessor half on updates.
    pub(crate) fn define_own_descriptor_slot(
        &mut self,
        object: Value,
        key: Value,
        src: Value,
        slot: DescriptorSlot,
    ) -> Result<(), EvalFailure> {
        let key = self.observable_property_key(key)?;
        let half = (src != Value::UNDEFINED).then_some(src);
        let updated = match (self.own_descriptor(object, &key)?, slot) {
            (None, DescriptorSlot::Value) => Property::Data {
                value: src,
                writable: true,
                enumerable: false,
                configurable: true,
            },
            (None, DescriptorSlot::Getter) => Property::Accessor {
                getter: half,
                setter: None,
                enumerable: false,
                configurable: true,
            },
            (None, DescriptorSlot::Setter) => Property::Accessor {
                getter: None,
                setter: half,
                enumerable: false,
                configurable: true,
            },
            (
                Some(Property::Data {
                    writable,
                    enumerable,
                    configurable,
                    ..
                }),
                DescriptorSlot::Value,
            ) => Property::Data {
                value: src,
                writable,
                enumerable,
                configurable,
            },
            (
                Some(Property::Accessor {
                    setter,
                    enumerable,
                    configurable,
                    ..
                }),
                DescriptorSlot::Getter,
            ) => Property::Accessor {
                getter: half,
                setter,
                enumerable,
                configurable,
            },
            (
                Some(Property::Accessor {
                    getter,
                    enumerable,
                    configurable,
                    ..
                }),
                DescriptorSlot::Setter,
            ) => Property::Accessor {
                getter,
                setter: half,
                enumerable,
                configurable,
            },
            _ => return Err(type_error("decorator replacement changes descriptor shape")),
        };
        self.define_descriptor(object, key, updated)
    }
}
fn property_definition_charges(
    properties: &PropertyMap,
    key: &PropertyKey,
    descriptor: &Property,
) -> (usize, usize) {
    match properties.get(key) {
        None => (
            key.charge_bytes().saturating_add(descriptor.charge_bytes()),
            0,
        ),
        Some(existing) => {
            let old = existing.charge_bytes();
            let new = descriptor.charge_bytes();
            if new >= old {
                (new.saturating_sub(old), 0)
            } else {
                (0, old.saturating_sub(new))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TestHost, blank_program};
    use super::*;
    use crate::Limits;

    fn inherited_message_getter<H: Host>(
        _machine: &mut Machine<'_, H>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Err(type_error(
            "inherited message getter ran before derived fields",
        ))
    }

    #[test]
    fn valita_style_message_getter_does_not_run_before_private_issue_field() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let getter = install_function(
            &mut machine.heap,
            &mut machine.intrinsics.builtins,
            "inherited message getter",
            0,
            inherited_message_getter::<TestHost>,
        );
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("message")),
            Property::Accessor {
                getter: Some(getter),
                setter: None,
                enumerable: false,
                configurable: true,
            },
        );
        let error = machine.intrinsics.global("Error").expect("Error exists");
        let error_prototype = machine
            .get_named_property(error, "prototype")
            .expect("Error.prototype exists");
        let prototype = machine
            .allocate(HeapEntry::Object {
                properties,
                prototype: Some(error_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("prototype allocation succeeds");
        let receiver = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("derived receiver allocation succeeds");
        let initialized = machine
            .call_value(error, receiver, &[])
            .expect("Error does not read the inherited message getter");
        assert_eq!(initialized, receiver);
        let stack = machine
            .get_named_property(receiver, "stack")
            .expect("Error creates stack on the existing receiver");
        assert!(
            machine
                .to_string(stack)
                .expect("stack is string")
                .eq_ascii("Error\n    at <bamts>")
        );

        let unrelated = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("unrelated receiver allocation succeeds");
        assert_ne!(
            machine
                .call_value(error, unrelated, &[])
                .expect("Error allocates for an unrelated receiver"),
            unrelated
        );

        let receiver_with_message = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("second derived receiver allocation succeeds");
        let message = machine
            .allocate(HeapEntry::String(EcmaString::encode("issue")))
            .expect("message allocation succeeds");
        assert_eq!(
            machine
                .call_value(error, receiver_with_message, &[message])
                .expect("Error initializes the second derived receiver"),
            receiver_with_message
        );
        let stack = machine
            .get_named_property(receiver_with_message, "stack")
            .expect("Error creates the second stack");
        assert!(
            machine
                .to_string(stack)
                .expect("stack is string")
                .eq_ascii("Error: issue\n    at <bamts>")
        );
    }

    #[test]
    fn global_infinity_descriptor_is_frozen_with_exact_value() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        let key = PropertyKey::Named(EcmaString::encode("Infinity"));
        let descriptor = machine
            .own_descriptor(global_this, &key)
            .expect("descriptor lookup succeeds")
            .expect("Infinity is defined on globalThis");
        match descriptor {
            Property::Data {
                value,
                writable,
                enumerable,
                configurable,
            } => {
                assert!(!writable, "Infinity must be non-writable");
                assert!(!enumerable, "Infinity must be non-enumerable");
                assert!(!configurable, "Infinity must be non-configurable");
                assert!(
                    value_number(value).is_infinite() && value_number(value).is_sign_positive(),
                    "Infinity value must be +Infinity"
                );
            }
            Property::Accessor { .. } => panic!("Infinity must be a data property"),
        }
    }

    #[test]
    fn global_nan_descriptor_is_frozen_with_exact_value() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        let key = PropertyKey::Named(EcmaString::encode("NaN"));
        let descriptor = machine
            .own_descriptor(global_this, &key)
            .expect("descriptor lookup succeeds")
            .expect("NaN is defined on globalThis");
        match descriptor {
            Property::Data {
                value,
                writable,
                enumerable,
                configurable,
            } => {
                assert!(!writable, "NaN must be non-writable");
                assert!(!enumerable, "NaN must be non-enumerable");
                assert!(!configurable, "NaN must be non-configurable");
                assert!(value_number(value).is_nan(), "NaN value must be NaN");
            }
            Property::Accessor { .. } => panic!("NaN must be a data property"),
        }
    }

    #[test]
    fn global_infinity_and_nan_are_stable_across_reads() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        let first_infinity = machine
            .get_named_property(global_this, "Infinity")
            .expect("Infinity is readable");
        let second_infinity = machine
            .get_named_property(global_this, "Infinity")
            .expect("Infinity is readable on second read");
        assert_eq!(
            first_infinity, second_infinity,
            "Infinity must be stable across reads"
        );
        let first_nan = machine
            .get_named_property(global_this, "NaN")
            .expect("NaN is readable");
        let second_nan = machine
            .get_named_property(global_this, "NaN")
            .expect("NaN is readable on second read");
        assert_eq!(first_nan, second_nan, "NaN must be stable across reads");
    }

    #[test]
    fn atomics_is_an_object_with_correct_to_string_tag() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let atomics = machine
            .intrinsics
            .global("Atomics")
            .expect("Atomics is installed");
        let object_to_string = machine.intrinsics.object_to_string();
        let result = machine
            .call_value(object_to_string, atomics, &[])
            .expect("Object.prototype.toString.call(Atomics) succeeds");
        assert!(
            machine
                .string_value(result)
                .is_some_and(|text| text.eq_ascii("[object Atomics]")),
            "Atomics must report [object Atomics]"
        );
    }

    #[test]
    fn atomics_to_string_tag_descriptor_matches_namespace_tag() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let atomics = machine
            .intrinsics
            .global("Atomics")
            .expect("Atomics is installed");
        let tag_key = PropertyKey::Symbol(heap_index(
            machine.intrinsics.builtins.symbol_to_string_tag(),
        ) as u32);
        let descriptor = machine
            .own_descriptor(atomics, &tag_key)
            .expect("descriptor lookup succeeds")
            .expect("Atomics has Symbol.toStringTag");
        match descriptor {
            Property::Data {
                value,
                writable,
                enumerable,
                configurable,
            } => {
                assert!(!writable, "Atomics toStringTag must be non-writable");
                assert!(!enumerable, "Atomics toStringTag must be non-enumerable");
                assert!(configurable, "Atomics toStringTag must be configurable");
                assert!(
                    machine
                        .string_value(value)
                        .is_some_and(|text| text.eq_ascii("Atomics")),
                    "Atomics toStringTag value must be 'Atomics'"
                );
            }
            Property::Accessor { .. } => panic!("Atomics toStringTag must be a data property"),
        }
    }

    #[test]
    fn atomics_global_binding_is_writable_and_configurable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        let key = PropertyKey::Named(EcmaString::encode("Atomics"));
        let descriptor = machine
            .own_descriptor(global_this, &key)
            .expect("descriptor lookup succeeds")
            .expect("Atomics is defined on globalThis");
        match descriptor {
            Property::Data {
                writable,
                enumerable,
                configurable,
                ..
            } => {
                assert!(writable, "Atomics global binding must be writable");
                assert!(!enumerable, "Atomics global binding must be non-enumerable");
                assert!(configurable, "Atomics global binding must be configurable");
            }
            Property::Accessor { .. } => panic!("Atomics global binding must be a data property"),
        }
    }

    #[test]
    fn atomics_claims_no_methods() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let atomics = machine
            .intrinsics
            .global("Atomics")
            .expect("Atomics is installed");
        let keys = machine
            .internal_own_property_keys(atomics)
            .expect("Atomics is an object");
        let method_keys: Vec<_> = keys
            .into_iter()
            .filter(|key| !matches!(key, PropertyKey::Symbol(_)))
            .collect();
        assert!(
            method_keys.is_empty(),
            "Atomics must not claim any named methods"
        );
    }

    #[test]
    fn copied_globals_are_non_enumerable_and_still_readable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        assert!(
            machine.enumerable_keys(global_this).unwrap().is_empty(),
            "fresh globalThis must not expose any copied globals to Object.keys/for...in"
        );
        for name in [
            "Atomics",
            "console",
            "process",
            "Object",
            "globalThis",
            "global",
        ] {
            assert!(
                machine.get_named_property(global_this, name).is_ok(),
                "{name} must still be directly accessible on globalThis"
            );
        }
    }

    #[test]
    fn suppressed_error_constructor_keeps_both_errors_non_enumerable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let constructor = machine
            .intrinsics
            .global("SuppressedError")
            .expect("SuppressedError exists");
        assert_eq!(
            machine.get_named_property(constructor, "length").unwrap(),
            Value::int32(3)
        );
        let error = Value::int32(11);
        let suppressed = Value::int32(22);
        let message = machine
            .allocate(HeapEntry::String(EcmaString::encode("outer")))
            .expect("message allocation succeeds");
        let result = machine
            .construct_value(constructor, &[error, suppressed, message])
            .expect("SuppressedError construction succeeds");

        assert_eq!(machine.get_named_property(result, "error").unwrap(), error);
        assert_eq!(
            machine.get_named_property(result, "suppressed").unwrap(),
            suppressed
        );
        let prototype = machine
            .get_named_property(constructor, "prototype")
            .unwrap();
        assert!(machine.inherits_from_prototype(result, prototype).unwrap());
        for (name, expected) in [("error", error), ("suppressed", suppressed)] {
            let descriptor = machine
                .own_descriptor(result, &PropertyKey::Named(EcmaString::encode(name)))
                .unwrap()
                .expect("SuppressedError has an own error field");
            assert!(matches!(
                descriptor,
                Property::Data {
                    value,
                    writable: true,
                    enumerable: false,
                    configurable: true,
                } if value == expected
            ));
        }
    }

    #[test]
    fn define_descriptor_create_delete_refunds_property_charge() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");
        let index = machine
            .runtime_slot(object)
            .expect("object slot lookup succeeds")
            .expect("object has a runtime slot");
        let data_key = PropertyKey::Named(EcmaString::encode("data"));
        let accessor_key = PropertyKey::Named(EcmaString::encode("accessor"));
        let baseline_slot = machine.slot_bytes[index];
        let baseline_heap = machine.heap_bytes;

        for _ in 0..3 {
            machine
                .define_descriptor(
                    object,
                    data_key.clone(),
                    Property::Data {
                        value: Value::int32(1),
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    },
                )
                .expect("data descriptor definition succeeds");
            assert!(
                machine
                    .internal_delete(object, &data_key)
                    .expect("delete succeeds"),
                "configurable data descriptor is removed"
            );
            machine
                .define_descriptor(
                    object,
                    accessor_key.clone(),
                    Property::Accessor {
                        getter: Some(Value::int32(2)),
                        setter: None,
                        enumerable: true,
                        configurable: true,
                    },
                )
                .expect("accessor descriptor definition succeeds");
            assert!(
                machine
                    .internal_delete(object, &accessor_key)
                    .expect("delete succeeds"),
                "configurable accessor descriptor is removed"
            );
            assert_eq!(machine.slot_bytes[index], baseline_slot);
            assert_eq!(machine.heap_bytes, baseline_heap);
            machine.assert_heap_ledger();
        }
    }

    #[test]
    fn define_descriptor_fails_before_charging_when_heap_limit_is_exhausted() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let limits = Limits {
            max_heap_bytes: 2,
            ..Limits::default()
        };
        let mut machine = Machine::new(&module, &mut host, limits);
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");
        let index = machine
            .runtime_slot(object)
            .expect("object slot lookup succeeds")
            .expect("object has a runtime slot");
        let before_slot = machine.slot_bytes[index];
        let before_heap = machine.heap_bytes;
        let key = PropertyKey::Named(EcmaString::encode("x"));

        assert!(matches!(
            machine.define_descriptor(
                object,
                key,
                Property::Data {
                    value: Value::int32(1),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            ),
            Err(EvalFailure::Runtime(
                crate::RuntimeErrorKind::HeapByteLimitExceeded { limit: 2 }
            ))
        ));
        assert_eq!(machine.slot_bytes[index], before_slot);
        assert_eq!(machine.heap_bytes, before_heap);
        machine.assert_heap_ledger();
    }

    #[test]
    fn load_own_descriptor_slot_covers_absent_data_accessor_and_shape_mismatch() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");
        let missing = allocate_string(&mut machine, EcmaString::encode("missing")).unwrap();
        let data_key = allocate_string(&mut machine, EcmaString::encode("data")).unwrap();
        let accessor_key =
            allocate_string(&mut machine, EcmaString::encode("accessor")).unwrap();
        let data_pk = PropertyKey::Named(EcmaString::encode("data"));
        let accessor_pk = PropertyKey::Named(EcmaString::encode("accessor"));
        let getter = Value::int32(1);
        let setter = Value::int32(2);

        assert_eq!(
            machine
                .load_own_descriptor_slot(object, missing, DescriptorSlot::Value)
                .unwrap(),
            Value::UNDEFINED,
            "absent property yields undefined"
        );

        machine
            .define_descriptor(
                object,
                data_pk.clone(),
                Property::Data {
                    value: Value::int32(7),
                    writable: false,
                    enumerable: true,
                    configurable: false,
                },
            )
            .unwrap();
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, data_key, DescriptorSlot::Value)
                .unwrap(),
            Value::int32(7)
        );
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, data_key, DescriptorSlot::Getter)
                .unwrap(),
            Value::UNDEFINED,
            "data/getter shape mismatch yields undefined"
        );

        machine
            .define_descriptor(
                object,
                accessor_pk,
                Property::Accessor {
                    getter: Some(getter),
                    setter: Some(setter),
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, accessor_key, DescriptorSlot::Getter)
                .unwrap(),
            getter
        );
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, accessor_key, DescriptorSlot::Setter)
                .unwrap(),
            setter
        );
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, accessor_key, DescriptorSlot::Value)
                .unwrap(),
            Value::UNDEFINED,
            "accessor/value shape mismatch yields undefined"
        );
    }

    #[test]
    fn define_own_descriptor_slot_creates_absent_preserves_attrs_and_rejects_shape_mismatch() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");
        let data_key = allocate_string(&mut machine, EcmaString::encode("data")).unwrap();
        let accessor_key =
            allocate_string(&mut machine, EcmaString::encode("accessor")).unwrap();
        let missing = allocate_string(&mut machine, EcmaString::encode("missing")).unwrap();
        let missing_getter =
            allocate_string(&mut machine, EcmaString::encode("missingGetter")).unwrap();
        let data_pk = PropertyKey::Named(EcmaString::encode("data"));
        let accessor_pk = PropertyKey::Named(EcmaString::encode("accessor"));
        let missing_pk = PropertyKey::Named(EcmaString::encode("missing"));
        let missing_getter_pk = PropertyKey::Named(EcmaString::encode("missingGetter"));
        let getter = Value::int32(11);
        let setter = Value::int32(22);
        let next_getter = Value::int32(33);
        let next_value = Value::int32(44);

        machine
            .define_own_descriptor_slot(object, missing, next_value, DescriptorSlot::Value)
            .unwrap();
        assert!(matches!(
            machine.own_descriptor(object, &missing_pk).unwrap(),
            Some(Property::Data {
                value,
                writable: true,
                enumerable: false,
                configurable: true,
            }) if value == next_value
        ));

        machine
            .define_own_descriptor_slot(object, missing_getter, getter, DescriptorSlot::Getter)
            .unwrap();
        assert!(matches!(
            machine.own_descriptor(object, &missing_getter_pk).unwrap(),
            Some(Property::Accessor {
                getter: Some(g),
                setter: None,
                enumerable: false,
                configurable: true,
            }) if g == getter
        ));

        machine
            .define_descriptor(
                object,
                data_pk.clone(),
                Property::Data {
                    value: Value::int32(1),
                    writable: false,
                    enumerable: true,
                    configurable: false,
                },
            )
            .unwrap();
        machine
            .define_own_descriptor_slot(object, data_key, next_value, DescriptorSlot::Value)
            .unwrap();
        assert!(matches!(
            machine.own_descriptor(object, &data_pk).unwrap(),
            Some(Property::Data {
                value,
                writable: false,
                enumerable: true,
                configurable: false,
            }) if value == next_value
        ));
        assert!(matches!(
            machine.define_own_descriptor_slot(object, data_key, getter, DescriptorSlot::Getter),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "decorator replacement changes descriptor shape"
            }))
        ));

        machine
            .define_descriptor(
                object,
                accessor_pk.clone(),
                Property::Accessor {
                    getter: Some(getter),
                    setter: Some(setter),
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        machine
            .define_own_descriptor_slot(object, accessor_key, next_getter, DescriptorSlot::Getter)
            .unwrap();
        assert!(matches!(
            machine.own_descriptor(object, &accessor_pk).unwrap(),
            Some(Property::Accessor {
                getter: Some(g),
                setter: Some(s),
                enumerable: false,
                configurable: true,
            }) if g == next_getter && s == setter
        ));
        assert!(matches!(
            machine.define_own_descriptor_slot(
                object,
                accessor_key,
                next_value,
                DescriptorSlot::Value
            ),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError {
                operation: "decorator replacement changes descriptor shape"
            }))
        ));
    }

    #[test]
    fn load_own_descriptor_slot_is_own_only_and_non_invoking() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let proto = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(proto),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        let inherited_key =
            allocate_string(&mut machine, EcmaString::encode("inherited")).unwrap();
        let accessor_key =
            allocate_string(&mut machine, EcmaString::encode("accessor")).unwrap();
        let inherited_pk = PropertyKey::Named(EcmaString::encode("inherited"));
        let accessor_pk = PropertyKey::Named(EcmaString::encode("accessor"));

        machine
            .define_descriptor(
                proto,
                inherited_pk,
                Property::Data {
                    value: Value::int32(99),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            )
            .unwrap();
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, inherited_key, DescriptorSlot::Value)
                .unwrap(),
            Value::UNDEFINED,
            "inherited prototype data must not be visible to own-slot load"
        );

        let getter = install_function(
            &mut machine.heap,
            &mut machine.intrinsics.builtins,
            "counting getter",
            0,
            |_machine, _this, _args, _constructing| {
                panic!("LoadOwnDescriptorSlot must not invoke accessors");
            },
        );
        machine
            .define_descriptor(
                object,
                accessor_pk,
                Property::Accessor {
                    getter: Some(getter),
                    setter: None,
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();
        assert_eq!(
            machine
                .load_own_descriptor_slot(object, accessor_key, DescriptorSlot::Getter)
                .unwrap(),
            getter,
            "Getter slot returns the getter function without calling it"
        );
    }

    #[test]
    fn descriptor_slot_helpers_coerce_key_objects_once_to_produced_property_key() {
        fn key_to_string<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            let calls = machine.get_named_property(this, "calls")?;
            let next = match calls.decode() {
                Some(Decoded::Int32(raw)) => raw.wrapping_add(1),
                _ => 1,
            };
            machine.set_data_property(this, "calls", Value::int32(next))?;
            let produced = machine.get_named_property(this, "produced")?;
            Ok(BuiltinOutcome::Value(produced))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");
        let produced =
            allocate_string(&mut machine, EcmaString::encode("produced-key")).unwrap();
        let produced_pk = PropertyKey::Named(EcmaString::encode("produced-key"));
        machine
            .define_descriptor(
                object,
                produced_pk.clone(),
                Property::Data {
                    value: Value::int32(7),
                    writable: true,
                    enumerable: false,
                    configurable: true,
                },
            )
            .unwrap();

        let key_object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        machine
            .set_data_property(key_object, "calls", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(key_object, "produced", produced)
            .unwrap();
        let to_string = install_function(
            &mut machine.heap,
            &mut machine.intrinsics.builtins,
            "descriptor key toString",
            0,
            key_to_string::<TestHost>,
        );
        machine
            .set_data_property(key_object, "toString", to_string)
            .unwrap();

        assert_eq!(
            machine
                .load_own_descriptor_slot(object, key_object, DescriptorSlot::Value)
                .unwrap(),
            Value::int32(7),
            "load must select the property key produced by key conversion"
        );
        assert_eq!(
            machine.get_named_property(key_object, "calls").unwrap(),
            Value::int32(1),
            "load must coerce the key object exactly once"
        );

        let define_key = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        machine
            .set_data_property(define_key, "calls", Value::int32(0))
            .unwrap();
        machine
            .set_data_property(define_key, "produced", produced)
            .unwrap();
        machine
            .set_data_property(define_key, "toString", to_string)
            .unwrap();
        machine
            .define_own_descriptor_slot(object, define_key, Value::int32(99), DescriptorSlot::Value)
            .unwrap();
        assert_eq!(
            machine.get_named_property(define_key, "calls").unwrap(),
            Value::int32(1),
            "define must coerce the key object exactly once"
        );
        assert!(
            matches!(
                machine.own_descriptor(object, &produced_pk).unwrap(),
                Some(Property::Data {
                    value,
                    writable: true,
                    enumerable: false,
                    configurable: true,
                }) if value == Value::int32(99)
            ),
            "define must write through the produced property key"
        );
    }
}
