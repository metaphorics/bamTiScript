use std::collections::BTreeMap;

use bamts_native::Value;

use super::{BuiltinDef, BuiltinOutcome, BuiltinTable, native_function, push};
use crate::{
    EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap, ThrowOrigin,
};

mod array;
mod json;
mod number;
mod object;
mod string;

pub(crate) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    object::install(heap, globals, builtins);
    array::install(heap, globals, builtins);
    string::install(heap, globals, builtins);
    number::install(heap, globals, builtins);
    install_boolean(heap, globals, builtins);
    install_math(heap, globals, builtins);
    install_globals(heap, globals, builtins);
    json::install(heap, globals, builtins);
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
    for name in [
        "Error",
        "EvalError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "TypeError",
        "URIError",
    ] {
        let prototype = push(
            heap,
            HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(builtins.object_prototype()),
                extensible: true,
                boxed_primitive: None,
            },
        );
        let constructor = install_function(heap, builtins, name, 1, error_constructor::<H>);
        builtins.set_constructor_prototype(heap, constructor, prototype);
        builtins.set_error_prototype(heap, constructor, prototype);
        globals.insert(name.to_owned(), constructor);
    }
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
    let prototype = machine.intrinsics.error_prototype(id);
    let object = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    if let Some(message) = args.first() {
        let text = allocate_string(machine, machine.to_string(*message)?)?;
        machine.set_data_property(object, "message", text)?;
    }
    Ok(BuiltinOutcome::Value(object))
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

    fn own_named_descriptor(
        &self,
        object: Value,
        name: &str,
    ) -> Result<Option<Property>, EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Ok(None);
        };
        let properties = match &self.heap[index] {
            HeapEntry::Object { properties, .. }
            | HeapEntry::Array { properties, .. }
            | HeapEntry::Function { properties, .. }
            | HeapEntry::NativeFunction { properties, .. }
            | HeapEntry::RegExp { properties, .. } => properties,
            _ => return Ok(None),
        };
        Ok(properties
            .get(&PropertyKey::Named(name.to_owned()))
            .cloned())
    }

    fn define_named_descriptor(
        &mut self,
        object: Value,
        name: String,
        descriptor: Property,
    ) -> Result<(), EvalFailure> {
        let Some(index) = self.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
            return Err(type_error("Object.defineProperty called on non-object"));
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
            _ => return Err(type_error("Object.defineProperty called on non-object")),
        };
        let key = PropertyKey::Named(name);
        if !*extensible && !properties.contains_key(&key) {
            return Err(type_error(
                "Cannot define property, object is not extensible",
            ));
        }
        properties.insert(key, descriptor);
        Ok(())
    }
}
