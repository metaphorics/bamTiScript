use std::collections::BTreeMap;

use bamts_native::Value;

use super::{BuiltinDef, BuiltinOutcome, BuiltinTable, native_function, push};
use crate::{
    EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap, ThrowOrigin,
};

mod array;
mod collections;
mod date;
mod json;
mod number;
mod object;
mod regexp;
mod string;
mod symbol;

pub(crate) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    symbol::install(heap, globals, builtins);
    collections::install_iterator_prototype(heap, globals, builtins);
    collections::install(heap, globals, builtins);
    date::install(heap, globals, builtins);
    object::install(heap, globals, builtins);
    array::install(heap, globals, builtins);
    string::install(heap, globals, builtins);
    number::install(heap, globals, builtins);
    install_boolean(heap, globals, builtins);
    install_math(heap, globals, builtins);
    regexp::install(heap, globals, builtins);
    install_globals(heap, globals, builtins);
    json::install(heap, globals, builtins);
    let json = *globals
        .get("JSON")
        .expect("JSON builtin installs its namespace");
    define_to_string_tag(heap, json, builtins.symbol_to_string_tag(), "JSON");
    install_errors(heap, globals, builtins);
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

fn define_data(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let index = heap_index(object);
    match &mut heap[index] {
        HeapEntry::Object { properties, .. }
        | HeapEntry::Array { properties, .. }
        | HeapEntry::Function { properties, .. }
        | HeapEntry::RegExp { properties, .. } => {
            properties.insert(PropertyKey::Named(name.to_owned()), builtin_property(value));
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
    let value = push(heap, HeapEntry::String(tag.to_owned()));
    let index = heap_index(object);
    let HeapEntry::Object { properties, .. } = &mut heap[index] else {
        panic!("namespace tag target must be an ordinary object");
    };
    properties.insert(
        PropertyKey::Private(heap_index(symbol) as u32),
        builtin_property(value),
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
    text: String,
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

fn install_boolean<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.boolean_prototype();
    let constructor = install_function(heap, builtins, "Boolean", 1, boolean_constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert("Boolean".to_owned(), constructor);
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
    globals: &mut BTreeMap<String, Value>,
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
        ("min", 2, math_min::<H>),
        ("max", 2, math_max::<H>),
        ("floor", 1, math_floor::<H>),
        ("ceil", 1, math_ceil::<H>),
        ("round", 1, math_round::<H>),
        ("trunc", 1, math_trunc::<H>),
        ("sign", 1, math_sign::<H>),
        ("pow", 2, math_pow::<H>),
        ("sqrt", 1, math_sqrt::<H>),
        ("random", 0, math_random::<H>),
        ("hypot", 2, math_hypot::<H>),
        ("log2", 1, math_log2::<H>),
        ("imul", 2, math_imul::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, math, name, function);
    }
    define_to_string_tag(heap, math, builtins.symbol_to_string_tag(), "Math");
    globals.insert("Math".to_owned(), math);
}

fn numeric_args<H: Host>(
    machine: &Machine<'_, H>,
    args: &[Value],
) -> Result<Vec<f64>, EvalFailure> {
    args.iter()
        .map(|value| machine.to_number(*value).map(value_number))
        .collect()
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
unary_math!(math_trunc, f64::trunc);
unary_math!(math_sqrt, f64::sqrt);
unary_math!(math_log2, f64::log2);

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

fn math_sign<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let x = value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let sign = if x == 0.0 || x.is_nan() {
        x
    } else {
        x.signum()
    };
    Ok(BuiltinOutcome::Value(crate::number_value(sign)))
}

fn math_min<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let numbers = numeric_args(machine, args)?;
    let value = numbers.into_iter().fold(f64::INFINITY, |a, b| {
        if a.is_nan() || b.is_nan() {
            f64::NAN
        } else if a == 0.0 && b == 0.0 {
            if a.is_sign_negative() || b.is_sign_negative() {
                -0.0
            } else {
                0.0
            }
        } else {
            a.min(b)
        }
    });
    Ok(BuiltinOutcome::Value(crate::number_value(value)))
}

fn math_max<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let numbers = numeric_args(machine, args)?;
    let value = numbers.into_iter().fold(f64::NEG_INFINITY, |a, b| {
        if a.is_nan() || b.is_nan() {
            f64::NAN
        } else if a == 0.0 && b == 0.0 {
            if a.is_sign_positive() || b.is_sign_positive() {
                0.0
            } else {
                -0.0
            }
        } else {
            a.max(b)
        }
    });
    Ok(BuiltinOutcome::Value(crate::number_value(value)))
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

fn math_hypot<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = numeric_args(machine, args)?;
    let max = values.iter().map(|x| x.abs()).fold(0.0, f64::max);
    let result = if max.is_infinite() {
        f64::INFINITY
    } else if values.iter().any(|x| x.is_nan()) {
        f64::NAN
    } else if max == 0.0 {
        0.0
    } else {
        max * values.iter().map(|x| (x / max).powi(2)).sum::<f64>().sqrt()
    };
    Ok(BuiltinOutcome::Value(crate::number_value(result)))
}

fn math_imul<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let a =
        value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?) as u32;
    let b =
        value_number(machine.to_number(args.get(1).copied().unwrap_or(Value::UNDEFINED))?) as u32;
    Ok(BuiltinOutcome::Value(Value::int32(a.wrapping_mul(b))))
}

fn install_globals<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
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
        ("encodeURIComponent", 1, string::encode_uri_component::<H>),
        ("decodeURIComponent", 1, string::decode_uri_component::<H>),
        ("structuredClone", 1, object::structured_clone::<H>),
    ] {
        let value = install_function(heap, builtins, name, length, handler);
        globals.insert(name.to_owned(), value);
    }
}

fn install_errors<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
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
        let length = usize::from(name == "AggregateError") as u32 + 1;
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
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
    name: &'static str,
    length: u32,
    prototype: Value,
    _error_prototype: Value,
) {
    let name_value = push(heap, HeapEntry::String(name.to_owned()));
    let empty = push(heap, HeapEntry::String(String::new()));
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named("name".to_owned()),
        Property::Data {
            value: name_value,
            writable: true,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        PropertyKey::Named("message".to_owned()),
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
    globals.insert(name.to_owned(), constructor);
}

fn error_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let id = machine
        .current_builtin_id()
        .ok_or_else(|| type_error("invalid error constructor"))?;
    let name = machine.intrinsics.builtins.get(id).name;
    let prototype = machine.intrinsics.error_prototype(id);
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    let (message_index, options_index) = if name == "AggregateError" {
        let errors = args.first().copied().unwrap_or(Value::UNDEFINED);
        let values = machine
            .array_elements(errors)?
            .ok_or_else(|| type_error("AggregateError errors argument is not iterable"))?;
        let array = allocate_array(machine, values)?;
        machine.set_data_property(object, "errors", array)?;
        (1, 2)
    } else {
        (0, 1)
    };
    if let Some(message) = args
        .get(message_index)
        .filter(|value| **value != Value::UNDEFINED)
    {
        let message_text = machine.to_string(*message)?;
        let text = allocate_string(machine, message_text)?;
        machine.set_data_property(object, "message", text)?;
    }
    if let Some(options) = args
        .get(options_index)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        let cause_key = PropertyKey::Named("cause".to_owned());
        if machine.has_property(options, &cause_key)? {
            let cause = machine.get_named_property(options, "cause")?;
            machine.set_data_property(object, "cause", cause)?;
        }
    }
    let message_value = machine.get_named_property(object, "message")?;
    let message = machine.to_string(message_value)?;
    let stack = if message.is_empty() {
        format!("{name}\n    at <bamts>")
    } else {
        format!("{name}: {message}\n    at <bamts>")
    };
    let stack = allocate_string(machine, stack)?;
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
        format!("{name}: {message}")
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}

impl<'a, H: Host> Machine<'a, H> {
    fn prototype_value(&self, object: Value) -> Result<Option<Value>, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
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

    fn set_prototype_value(
        &mut self,
        object: Value,
        prototype: Option<Value>,
    ) -> Result<(), EvalFailure> {
        self.set_prototype(object, prototype.unwrap_or(Value::NULL))
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

    fn mark_frozen(&mut self, value: Value) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(());
        };
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
            } => (properties, extensible),
            HeapEntry::ProcessEnv { extensible, .. } => {
                *extensible = false;
                return Ok(());
            }
            _ => return Ok(()),
        };
        *extensible = false;
        for (_, property) in &mut properties.0 {
            match property {
                Property::Data {
                    writable,
                    configurable,
                    ..
                } => {
                    *writable = false;
                    *configurable = false;
                }
                Property::Accessor { configurable, .. } => *configurable = false,
            }
        }
        Ok(())
    }

    fn is_frozen_value(&self, value: Value) -> Result<bool, EvalFailure> {
        let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
            return Ok(true);
        };
        let (properties, extensible) = match &self.heap[index] {
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
            } => (properties, extensible),
            HeapEntry::ProcessEnv { extensible, .. } => return Ok(!extensible),
            _ => return Ok(true),
        };
        Ok(!extensible
            && properties.iter().all(|(_, property)| match property {
                Property::Data {
                    writable,
                    configurable,
                    ..
                } => !writable && !configurable,
                Property::Accessor { configurable, .. } => !configurable,
            }))
    }

    pub(crate) fn own_descriptor(
        &self,
        object: Value,
        key: &PropertyKey,
    ) -> Result<Option<Property>, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
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
                ..
            } => {
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
            HeapEntry::Object { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. } => properties,
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
        let (properties, extensible, exists) = match &mut self.heap[index] {
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
            } => {
                let exists = properties.contains_key(&key);
                (properties, extensible, exists)
            }
            HeapEntry::Array {
                elements,
                properties,
                extensible,
                ..
            } => {
                let array_index = key
                    .as_str()
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
                if let Some(offset) = array_index {
                    if elements.len() <= offset {
                        elements.resize(offset + 1, Value::HOLE);
                    }
                    elements[offset] = Value::HOLE;
                }
                (properties, extensible, exists)
            }
            _ => return Err(type_error("Object.defineProperty called on non-object")),
        };
        if !*extensible && !exists {
            return Err(type_error(
                "Cannot define property, object is not extensible",
            ));
        }
        properties.insert(key, descriptor);
        Ok(())
    }
}
