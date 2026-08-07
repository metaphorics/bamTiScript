use std::cmp::Ordering;
use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, to_integer_or_infinity,
    type_error, value_number,
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
    let prototype = builtins.array_prototype();
    let constructor = install_function(heap, builtins, "Array", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::from_utf8("Array"), constructor);
    for (name, length, handler) in [
        ("isArray", 1, is_array::<H> as BuiltinHandler<H>),
        ("from", 1, from::<H>),
        ("of", 0, of::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_static(heap, constructor, name, function);
    }
    for (name, length, handler) in [
        ("push", 1, push::<H> as BuiltinHandler<H>),
        ("pop", 0, pop::<H>),
        ("shift", 0, shift::<H>),
        ("unshift", 1, unshift::<H>),
        ("slice", 2, slice::<H>),
        ("splice", 2, splice::<H>),
        ("concat", 1, concat::<H>),
        ("join", 1, join::<H>),
        ("indexOf", 1, index_of::<H>),
        ("lastIndexOf", 1, last_index_of::<H>),
        ("includes", 1, includes::<H>),
        ("find", 1, find::<H>),
        ("findIndex", 1, find_index::<H>),
        ("findLast", 1, find_last::<H>),
        ("filter", 1, filter::<H>),
        ("map", 1, map::<H>),
        ("forEach", 1, for_each::<H>),
        ("reduce", 1, reduce::<H>),
        ("reduceRight", 1, reduce_right::<H>),
        ("some", 1, some::<H>),
        ("every", 1, every::<H>),
        ("sort", 1, sort::<H>),
        ("reverse", 0, reverse::<H>),
        ("flat", 0, flat::<H>),
        ("flatMap", 1, flat_map::<H>),
        ("fill", 1, fill::<H>),
        ("at", 1, at::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
    }
    let keys = install_function(heap, builtins, "keys", 0, keys_iterator::<H>);
    let values = install_function(heap, builtins, "values", 0, values_iterator::<H>);
    let entries = install_function(heap, builtins, "entries", 0, entries_iterator::<H>);
    define_data(heap, prototype, "keys", keys);
    define_data(heap, prototype, "values", values);
    define_data(heap, prototype, "entries", entries);
    let unscopables = super::super::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: None,
            extensible: true,
            boxed_primitive: None,
        },
    );
    {
        let HeapEntry::Object { properties, .. } = &mut heap[super::heap_index(unscopables)] else {
            unreachable!()
        };
        for name in [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ] {
            properties.insert(
                PropertyKey::Named(EcmaString::from_utf8(name)),
                Property::Data {
                    value: Value::TRUE,
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            );
        }
    }
    let HeapEntry::Array { properties, .. } = &mut heap[super::heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Symbol(super::heap_index(builtins.symbol_iterator()) as u32),
        super::builtin_property(values),
    );
    properties.insert(
        PropertyKey::Symbol(super::heap_index(builtins.symbol_unscopables()) as u32),
        Property::Data {
            value: unscopables,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
}

fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("Array constructor must be native")
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
    let elements = if args.len() == 1 {
        let n = value_number(args[0]);
        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u32::MAX as f64 {
            vec![Value::HOLE; n as usize]
        } else {
            args.to_vec()
        }
    } else {
        args.to_vec()
    };
    Ok(BuiltinOutcome::Value(allocate_array(machine, elements)?))
}
fn is_array<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(Value::boolean(
        machine
            .array_elements(args.first().copied().unwrap_or(Value::UNDEFINED))?
            .is_some(),
    )))
}
fn from<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = args.first().copied().unwrap_or(Value::UNDEFINED);
    let callback = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED);
    if let Some(callback) = callback
        && !machine.is_callable(callback)?
    {
        return Err(type_error("Array.from mapper is not callable"));
    }
    let mut elements = machine.iterable_values(source)?;
    if let Some(callback) = callback {
        let this_arg = args.get(2).copied().unwrap_or(Value::UNDEFINED);
        for (index, element) in elements.iter_mut().enumerate() {
            *element = machine.call_value(
                callback,
                this_arg,
                &[*element, crate::number_value(index as f64)],
            )?;
        }
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, elements)?))
}
fn of<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(allocate_array(
        machine,
        args.to_vec(),
    )?))
}
fn elements<H: Host>(machine: &Machine<'_, H>, this: Value) -> Result<Vec<Value>, EvalFailure> {
    machine
        .array_elements(this)?
        .ok_or_else(|| type_error("Array method called on incompatible receiver"))
}
/// Validates that `this` is an Array receiver without materializing its
/// elements. The iterator factories only need receiver validation — the
/// actual elements are read lazily by `iterator_next` — so cloning the
/// whole `Vec<Value>` (as `elements` does) is wasted allocation. Mirrors
/// `collection_slot` in `collections.rs`.
fn array_slot<H: Host>(machine: &Machine<'_, H>, object: Value) -> Result<(), EvalFailure> {
    let Some(index) = machine.runtime_slot(object).map_err(EvalFailure::Runtime)? else {
        return Err(type_error("Array method called on incompatible receiver"));
    };
    if !matches!(machine.heap[index], HeapEntry::Array { .. }) {
        return Err(type_error("Array method called on incompatible receiver"));
    }
    Ok(())
}
fn write_elements<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    values: Vec<Value>,
) -> Result<(), EvalFailure> {
    machine.replace_array_elements(this, values)
}
fn push<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    values.extend_from_slice(args);
    let len = values.len();
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(crate::number_value(len as f64)))
}
fn pop<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    let value = values
        .pop()
        .filter(|v| *v != Value::HOLE)
        .unwrap_or(Value::UNDEFINED);
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(value))
}
fn shift<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    let value = if values.is_empty() {
        Value::UNDEFINED
    } else {
        values.remove(0)
    };
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(if value == Value::HOLE {
        Value::UNDEFINED
    } else {
        value
    }))
}
fn unshift<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let old = elements(machine, this)?;
    let mut values = Vec::with_capacity(args.len() + old.len());
    values.extend_from_slice(args);
    values.extend(old);
    let len = values.len();
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(crate::number_value(len as f64)))
}
fn relative_index<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
    len: usize,
) -> Result<usize, EvalFailure> {
    let n = to_integer_or_infinity(machine, value)?;
    Ok(if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        n.min(len as f64) as usize
    })
}
fn slice<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let start = relative_index(
        machine,
        args.first().copied().unwrap_or(Value::int32(0)),
        values.len(),
    )?;
    let end = relative_index(
        machine,
        args.get(1)
            .copied()
            .unwrap_or(crate::number_value(values.len() as f64)),
        values.len(),
    )?;
    Ok(BuiltinOutcome::Value(allocate_array(
        machine,
        if end > start {
            values[start..end].to_vec()
        } else {
            Vec::new()
        },
    )?))
}
fn splice<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    let start = relative_index(
        machine,
        args.first().copied().unwrap_or(Value::int32(0)),
        values.len(),
    )?;
    let delete = if args.len() < 2 {
        values.len() - start
    } else {
        to_integer_or_infinity(machine, args[1])?
            .max(0.0)
            .min((values.len() - start) as f64) as usize
    };
    let removed: Vec<_> = values
        .splice(start..start + delete, args.iter().copied().skip(2))
        .collect();
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(allocate_array(machine, removed)?))
}
fn concat<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut out = elements(machine, this)?;
    for arg in args {
        if let Some(values) = machine.array_elements(*arg)? {
            out.extend(values)
        } else {
            out.push(*arg)
        }
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, out)?))
}
fn join<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let separator = if args.is_empty() || args[0] == Value::UNDEFINED {
        EcmaString::from_utf8(",")
    } else {
        machine.to_string(args[0])?
    };
    let mut output = bamts_bytecode::EcmaStringBuilder::new();
    for (index, value) in values.into_iter().enumerate() {
        if index != 0 {
            for &unit in separator.as_units() {
                output.push_unit(unit);
            }
        }
        if value != Value::HOLE
            && !matches!(value.decode(), Some(Decoded::Undefined | Decoded::Null))
        {
            for &unit in machine.to_string(value)?.as_units() {
                output.push_unit(unit);
            }
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn from_index<H: Host>(
    machine: &Machine<'_, H>,
    arg: Option<Value>,
    len: usize,
    reverse: bool,
) -> Result<Option<usize>, EvalFailure> {
    let default = if reverse { len.saturating_sub(1) } else { 0 };
    let Some(value) = arg else {
        return Ok((len > 0).then_some(default));
    };
    let n = to_integer_or_infinity(machine, value)?;
    if n >= len as f64 {
        return Ok(None);
    };
    if n < -(len as f64) {
        return Ok((!reverse && len > 0).then_some(0));
    }
    Ok(Some(if n < 0.0 {
        (len as f64 + n) as usize
    } else {
        n as usize
    }))
}
fn index_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let needle = args.first().copied().unwrap_or(Value::UNDEFINED);
    let found = match from_index(machine, args.get(1).copied(), values.len(), false)? {
        Some(start) => values
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, v)| **v != Value::HOLE && machine.strict_equal(**v, needle))
            .map(|(i, _)| i as f64)
            .unwrap_or(-1.0),
        None => -1.0,
    };
    Ok(BuiltinOutcome::Value(crate::number_value(found)))
}
fn last_index_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let needle = args.first().copied().unwrap_or(Value::UNDEFINED);
    let start = from_index(machine, args.get(1).copied(), values.len(), true)?;
    let found = start
        .and_then(|s| {
            (0..=s)
                .rev()
                .find(|i| values[*i] != Value::HOLE && machine.strict_equal(values[*i], needle))
        })
        .map(|i| i as f64)
        .unwrap_or(-1.0);
    Ok(BuiltinOutcome::Value(crate::number_value(found)))
}
fn includes<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let needle = args.first().copied().unwrap_or(Value::UNDEFINED);
    let found =
        from_index(machine, args.get(1).copied(), values.len(), false)?.is_some_and(|start| {
            values.iter().skip(start).any(|v| {
                machine.same_value_zero(
                    if *v == Value::HOLE {
                        Value::UNDEFINED
                    } else {
                        *v
                    },
                    needle,
                )
            })
        });
    Ok(BuiltinOutcome::Value(Value::boolean(found)))
}
fn callback(args: &[Value]) -> Result<Value, EvalFailure> {
    args.first()
        .copied()
        .filter(|v| *v != Value::UNDEFINED)
        .ok_or_else(|| type_error("callback is not a function"))
}
fn find<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for (i, v) in values.iter().enumerate() {
        let value = if *v == Value::HOLE {
            Value::UNDEFINED
        } else {
            *v
        };
        if machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[value, crate::number_value(i as f64), this],
        )? {
            return Ok(BuiltinOutcome::Value(value));
        }
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}
fn find_index<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for (i, v) in values.iter().enumerate() {
        let value = if *v == Value::HOLE {
            Value::UNDEFINED
        } else {
            *v
        };
        if machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[value, crate::number_value(i as f64), this],
        )? {
            return Ok(BuiltinOutcome::Value(crate::number_value(i as f64)));
        }
    }
    Ok(BuiltinOutcome::Value(crate::number_value(-1.0)))
}
fn find_last<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for i in (0..values.len()).rev() {
        let value = if values[i] == Value::HOLE {
            Value::UNDEFINED
        } else {
            values[i]
        };
        if machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[value, crate::number_value(i as f64), this],
        )? {
            return Ok(BuiltinOutcome::Value(value));
        }
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}
fn filter<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    let mut out = Vec::new();
    for (i, v) in values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != Value::HOLE)
    {
        if machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[*v, crate::number_value(i as f64), this],
        )? {
            out.push(*v)
        }
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, out)?))
}
fn map<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    let mut out = vec![Value::HOLE; values.len()];
    for (i, v) in values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != Value::HOLE)
    {
        out[i] = machine.call_value(
            cb,
            Value::UNDEFINED,
            &[*v, crate::number_value(i as f64), this],
        )?
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, out)?))
}
fn for_each<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for (i, v) in values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != Value::HOLE)
    {
        machine.call_value(
            cb,
            Value::UNDEFINED,
            &[*v, crate::number_value(i as f64), this],
        )?;
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}
fn reduce_impl<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    reverse: bool,
) -> Result<Value, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    let indices: Vec<_> = if reverse {
        (0..values.len()).rev().collect()
    } else {
        (0..values.len()).collect()
    };
    let mut iter = indices.into_iter().filter(|i| values[*i] != Value::HOLE);
    let mut acc = if let Some(initial) = args.get(1) {
        *initial
    } else {
        let i = iter
            .next()
            .ok_or_else(|| type_error("Reduce of empty array with no initial value"))?;
        values[i]
    };
    for i in iter {
        acc = machine.call_value(
            cb,
            Value::UNDEFINED,
            &[acc, values[i], crate::number_value(i as f64), this],
        )?
    }
    Ok(acc)
}
fn reduce<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(reduce_impl(
        machine, this, args, false,
    )?))
}
fn reduce_right<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(reduce_impl(
        machine, this, args, true,
    )?))
}
fn some<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for (i, v) in values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != Value::HOLE)
    {
        if machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[*v, crate::number_value(i as f64), this],
        )? {
            return Ok(BuiltinOutcome::Value(Value::TRUE));
        }
    }
    Ok(BuiltinOutcome::Value(Value::FALSE))
}
fn every<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let cb = callback(args)?;
    for (i, v) in values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != Value::HOLE)
    {
        if !machine.call_truthy(
            cb,
            Value::UNDEFINED,
            &[*v, crate::number_value(i as f64), this],
        )? {
            return Ok(BuiltinOutcome::Value(Value::FALSE));
        }
    }
    Ok(BuiltinOutcome::Value(Value::TRUE))
}
fn sort<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let comparator = args.first().copied().filter(|v| *v != Value::UNDEFINED);
    let mut present: Vec<(usize, Value)> = values
        .into_iter()
        .enumerate()
        .filter(|(_, v)| *v != Value::HOLE)
        .collect();
    let mut error = None;
    present.sort_by(|(ia, a), (ib, b)| {
        if error.is_some() {
            return Ordering::Equal;
        }
        let ordering = if *a == Value::UNDEFINED {
            if *b == Value::UNDEFINED {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        } else if *b == Value::UNDEFINED {
            Ordering::Less
        } else if let Some(cb) = comparator {
            match machine
                .call_value(cb, Value::UNDEFINED, &[*a, *b])
                .and_then(|v| machine.to_number(v))
            {
                Ok(v) => value_number(v).partial_cmp(&0.0).unwrap_or(Ordering::Equal),
                Err(e) => {
                    error = Some(e);
                    Ordering::Equal
                }
            }
        } else {
            match (machine.to_string(*a), machine.to_string(*b)) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Err(e), _) | (_, Err(e)) => {
                    error = Some(e);
                    Ordering::Equal
                }
            }
        };
        ordering.then_with(|| ia.cmp(ib))
    });
    if let Some(e) = error {
        return Err(e);
    }
    let len = present.len();
    let mut out: Vec<_> = present.into_iter().map(|(_, v)| v).collect();
    out.resize(machine.array_length(this)?, Value::HOLE);
    debug_assert!(len <= out.len());
    write_elements(machine, this, out)?;
    Ok(BuiltinOutcome::Value(this))
}
fn reverse<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    values.reverse();
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(this))
}
fn flatten<H: Host>(
    machine: &Machine<'_, H>,
    values: Vec<Value>,
    depth: usize,
    out: &mut Vec<Value>,
) -> Result<(), EvalFailure> {
    for value in values {
        if depth > 0
            && let Some(inner) = machine.array_elements(value)?
        {
            flatten(machine, inner, depth - 1, out)?;
            continue;
        }
        if value != Value::HOLE {
            out.push(value)
        }
    }
    Ok(())
}
fn flat<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let depth = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::int32(1)))?
        .max(0.0) as usize;
    let mut out = Vec::new();
    flatten(machine, values, depth, &mut out)?;
    Ok(BuiltinOutcome::Value(allocate_array(machine, out)?))
}
fn flat_map<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let BuiltinOutcome::Value(mapped) = map(machine, this, args, false)? else {
        unreachable!()
    };
    flat(machine, mapped, &[Value::int32(1)], false)
}
fn fill<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut values = elements(machine, this)?;
    let start = relative_index(
        machine,
        args.get(1).copied().unwrap_or(Value::int32(0)),
        values.len(),
    )?;
    let end = relative_index(
        machine,
        args.get(2)
            .copied()
            .unwrap_or(crate::number_value(values.len() as f64)),
        values.len(),
    )?;
    for slot in values.iter_mut().take(end).skip(start) {
        *slot = args.first().copied().unwrap_or(Value::UNDEFINED)
    }
    write_elements(machine, this, values)?;
    Ok(BuiltinOutcome::Value(this))
}
fn at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = elements(machine, this)?;
    let n = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let index = if n < 0.0 { values.len() as f64 + n } else { n };
    let value = if index < 0.0 || index >= values.len() as f64 {
        Value::UNDEFINED
    } else {
        let v = values[index as usize];
        if v == Value::HOLE {
            Value::UNDEFINED
        } else {
            v
        }
    };
    Ok(BuiltinOutcome::Value(value))
}

fn keys_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    array_slot(machine, this)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine,
        this,
        IterationKind::Key,
    )?))
}

fn values_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    array_slot(machine, this)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine,
        this,
        IterationKind::Value,
    )?))
}

fn entries_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    array_slot(machine, this)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine,
        this,
        IterationKind::Entry,
    )?))
}

#[cfg(test)]
mod tests {
    use bamts_native::Decoded;

    use super::super::test_support::{TestHost, blank_program, custom_iterable, ordinary_object};
    use super::*;
    use crate::Limits;
    use crate::intrinsics::BuiltinDef;

    fn call_array(
        machine: &mut Machine<'_, TestHost>,
        method_name: &str,
        args: &[Value],
    ) -> Result<Value, EvalFailure> {
        let constructor = machine.intrinsics.global("Array").unwrap();
        let method = machine.get_named_property(constructor, method_name)?;
        machine.call_value(method, constructor, args)
    }

    // ---- custom iterable helpers -------------------------------------------

    /// Builds an object with a custom `Symbol.iterator` that yields `values`
    /// in order, bypassing any structural shortcuts.

    // ---- tests -------------------------------------------------------------

    #[test]
    fn array_from_observes_custom_iterator_override() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // An array with real elements [1, 2, 3] but a custom Symbol.iterator
        // that yields [10, 20, 30]. Array.from must observe the override.
        let backing = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::int32(2), Value::int32(3)],
        )
        .unwrap();
        let source = custom_iterable(
            &mut machine,
            vec![Value::int32(10), Value::int32(20), Value::int32(30)],
        );
        // Copy the fixture state the custom iterator needs from `source` onto
        // `backing` before installing the custom Symbol.iterator.
        let values = machine.get_named_property(source, "_values").unwrap();
        let next = machine.get_named_property(source, "_next").unwrap();
        machine
            .set_data_property(backing, "_values", values)
            .unwrap();
        machine.set_data_property(backing, "_next", next).unwrap();

        let iterator_symbol = machine.intrinsics.builtins.symbol_iterator();
        let iterator_key = machine.to_property_key(iterator_symbol).unwrap();
        let custom_iter_fn = machine.get_property_key(source, &iterator_key).unwrap();
        machine
            .set_data_property_key(backing, iterator_key, custom_iter_fn)
            .unwrap();

        let result = call_array(&mut machine, "from", &[backing]).unwrap();
        let elements = machine.array_elements(result).unwrap().unwrap();
        assert_eq!(
            elements,
            vec![Value::int32(10), Value::int32(20), Value::int32(30)]
        );
    }

    #[test]
    fn array_from_preserves_mapper_order_and_thisarg() {
        fn mapper<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            args: &[Value],
            _constructing: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            let element = args.first().copied().unwrap_or(Value::int32(0));
            let index = args.get(1).copied().unwrap_or(Value::int32(0));
            let offset = machine.get_named_property(this, "offset")?;
            let e = match element.decode() {
                Some(Decoded::Int32(i)) => i,
                _ => 0,
            };
            let i = match index.decode() {
                Some(Decoded::Int32(i)) => i,
                _ => 0,
            };
            let o = match offset.decode() {
                Some(Decoded::Int32(i)) => i,
                _ => 0,
            };
            Ok(BuiltinOutcome::Value(Value::int32(e * 100 + i + o)))
        }

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = custom_iterable(
            &mut machine,
            vec![Value::int32(1), Value::int32(2), Value::int32(3)],
        );
        let mapper_id = machine.intrinsics.builtins.register(BuiltinDef {
            name: "mapper",
            length: 1,
            handler: mapper::<TestHost>,
        });
        let mapper_fn =
            crate::intrinsics::native_function(&mut machine.heap, mapper_id, "mapper", 1);
        let this_arg = ordinary_object(&mut machine);
        machine
            .set_data_property(this_arg, "offset", Value::int32(1000))
            .unwrap();

        let result = call_array(&mut machine, "from", &[source, mapper_fn, this_arg]).unwrap();
        let elements = machine.array_elements(result).unwrap().unwrap();
        // element*100 + index + offset(1000)
        assert_eq!(
            elements,
            vec![
                Value::int32(1100), // 1*100 + 0 + 1000
                Value::int32(1201), // 2*100 + 1 + 1000
                Value::int32(1302), // 3*100 + 2 + 1000
            ]
        );
    }

    #[test]
    fn array_from_consumes_string_through_protocol() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let text = allocate_string(&mut machine, EcmaString::from_utf8("abc")).unwrap();
        let result = call_array(&mut machine, "from", &[text]).unwrap();
        let elements = machine.array_elements(result).unwrap().unwrap();
        assert_eq!(elements.len(), 3);
        assert!(
            machine
                .string_value(elements[0])
                .is_some_and(|s| s.eq_ascii("a"))
        );
        assert!(
            machine
                .string_value(elements[1])
                .is_some_and(|s| s.eq_ascii("b"))
        );
        assert!(
            machine
                .string_value(elements[2])
                .is_some_and(|s| s.eq_ascii("c"))
        );
    }

    #[test]
    fn array_from_rejects_non_iterable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = ordinary_object(&mut machine); // no Symbol.iterator
        let result = call_array(&mut machine, "from", &[source]);
        assert!(result.is_err());
    }

    #[test]
    fn array_prototype_unscopables_descriptor_and_entries() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array_prototype = machine.intrinsics.array_prototype;
        let unscopables_symbol = machine.intrinsics.builtins.symbol_unscopables();
        let key = machine.to_property_key(unscopables_symbol).unwrap();
        let descriptor = machine
            .own_descriptor(array_prototype, &key)
            .expect("descriptor lookup succeeds")
            .expect("Array.prototype[Symbol.unscopables] is defined");
        let Property::Data {
            value: unscopables,
            writable,
            enumerable,
            configurable,
        } = descriptor
        else {
            panic!("Array.prototype[Symbol.unscopables] must be a data property");
        };
        assert!(
            !writable,
            "Array.prototype[Symbol.unscopables] must be non-writable"
        );
        assert!(
            !enumerable,
            "Array.prototype[Symbol.unscopables] must be non-enumerable"
        );
        assert!(
            configurable,
            "Array.prototype[Symbol.unscopables] must be configurable"
        );
        assert_eq!(
            machine.prototype_value(unscopables).unwrap(),
            None,
            "unscopables object must have a null prototype"
        );
        let names = [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ];
        for name in names {
            let entry = machine
                .get_named_property(unscopables, name)
                .unwrap_or_else(|_| panic!("{name} must be present on unscopables"));
            assert_eq!(entry, Value::TRUE, "{name} must be true");
            let entry_key = PropertyKey::Named(EcmaString::from_utf8(name));
            let entry_descriptor = machine
                .own_descriptor(unscopables, &entry_key)
                .expect("entry descriptor lookup succeeds")
                .unwrap_or_else(|| panic!("{name} must have an own descriptor"));
            assert!(
                matches!(
                    entry_descriptor,
                    Property::Data {
                        value: Value::TRUE,
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    }
                ),
                "{name} must be a CreateDataProperty true entry"
            );
        }
        let expected: Vec<_> = names
            .into_iter()
            .map(EcmaString::from_utf8)
            .map(PropertyKey::Named)
            .collect();
        assert_eq!(
            machine.own_property_keys(unscopables).unwrap(),
            expected,
            "unscopables own keys must be exactly the standard entry set"
        );
    }

    // ---- iterator factory regression tests --------------------------------

    /// Calls an `Array.prototype` method (e.g. `keys`, `values`, `entries`)
    /// on `this_array` and returns the result.
    fn call_proto(
        machine: &mut Machine<'_, TestHost>,
        method_name: &str,
        this_array: Value,
    ) -> Value {
        let proto = machine.intrinsics.array_prototype;
        let method = machine.get_named_property(proto, method_name).unwrap();
        machine.call_value(method, this_array, &[]).unwrap()
    }

    /// Drives `iterator`'s `next()` once and returns `(done, value)`.
    fn iter_next(machine: &mut Machine<'_, TestHost>, iterator: Value) -> (bool, Value) {
        let next_fn = machine.get_named_property(iterator, "next").unwrap();
        let result = machine.call_value(next_fn, iterator, &[]).unwrap();
        let done = machine.get_named_property(result, "done").unwrap();
        let value = machine.get_named_property(result, "value").unwrap();
        let is_done = matches!(done.decode(), Some(Decoded::Boolean(true)));
        (is_done, value)
    }

    #[test]
    fn keys_iterator_order_and_holes() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // [1, <hole>, 3] — holes still produce a key index.
        let array = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::HOLE, Value::int32(3)],
        )
        .unwrap();

        let iter = call_proto(&mut machine, "keys", array);
        let (done, v0) = iter_next(&mut machine, iter);
        assert!(!done);
        assert!(matches!(v0.decode(), Some(Decoded::Int32(n)) if n == 0));
        let (done, v1) = iter_next(&mut machine, iter);
        assert!(!done);
        assert!(matches!(v1.decode(), Some(Decoded::Int32(n)) if n == 1));
        let (done, v2) = iter_next(&mut machine, iter);
        assert!(!done);
        assert!(matches!(v2.decode(), Some(Decoded::Int32(n)) if n == 2));
        let (done, _) = iter_next(&mut machine, iter);
        assert!(done);
    }

    #[test]
    fn values_iterator_order_and_holes() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // [1, <hole>, 3] — the hole yields `undefined`, not skipped.
        let array = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::HOLE, Value::int32(3)],
        )
        .unwrap();

        let iter = call_proto(&mut machine, "values", array);
        let (done, v0) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v0, Value::int32(1));
        let (done, v1) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v1, Value::UNDEFINED);
        let (done, v2) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v2, Value::int32(3));
        let (done, _) = iter_next(&mut machine, iter);
        assert!(done);
    }

    #[test]
    fn entries_iterator_order_and_holes() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // [1, <hole>, 3] — entries yield [index, value] including the hole.
        let array = allocate_array(
            &mut machine,
            vec![Value::int32(1), Value::HOLE, Value::int32(3)],
        )
        .unwrap();

        let iter = call_proto(&mut machine, "entries", array);

        let (done, entry) = iter_next(&mut machine, iter);
        assert!(!done);
        let pair = machine.array_elements(entry).unwrap().unwrap();
        assert!(matches!(pair[0].decode(), Some(Decoded::Int32(n)) if n == 0));
        assert_eq!(pair[1], Value::int32(1));

        let (done, entry) = iter_next(&mut machine, iter);
        assert!(!done);
        let pair = machine.array_elements(entry).unwrap().unwrap();
        assert!(matches!(pair[0].decode(), Some(Decoded::Int32(n)) if n == 1));
        assert_eq!(pair[1], Value::UNDEFINED);

        let (done, entry) = iter_next(&mut machine, iter);
        assert!(!done);
        let pair = machine.array_elements(entry).unwrap().unwrap();
        assert!(matches!(pair[0].decode(), Some(Decoded::Int32(n)) if n == 2));
        assert_eq!(pair[1], Value::int32(3));

        let (done, _) = iter_next(&mut machine, iter);
        assert!(done);
    }

    #[test]
    fn iterator_rejects_non_array_receiver() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let obj = ordinary_object(&mut machine);
        let proto = machine.intrinsics.array_prototype;
        let method = machine.get_named_property(proto, "keys").unwrap();
        let result = machine.call_value(method, obj, &[]);
        assert!(result.is_err(), "keys() on a non-array must throw");
    }

    /// Spec: `%ArrayIteratorPrototype%.next` reads the source array live on
    /// each call, so mutations between `next()` calls are visible. The
    /// iterator captures the original length at creation time; elements
    /// added beyond that length are not visited, but in-bounds mutations
    /// are observed. Our `BuiltinIterator` reads `elements` by cursor each
    /// step, so in-bounds replacement is visible immediately.
    #[test]
    fn values_iterator_observes_mutation_during_iteration() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let array = allocate_array(
            &mut machine,
            vec![Value::int32(10), Value::int32(20), Value::int32(30)],
        )
        .unwrap();

        let iter = call_proto(&mut machine, "values", array);

        // Consume first element (10).
        let (done, v0) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v0, Value::int32(10));

        // Mutate index 1 from 20 to 99 — the iterator must see 99, not 20.
        machine
            .set_data_property(array, "1", Value::int32(99))
            .unwrap();

        let (done, v1) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v1, Value::int32(99));

        let (done, v2) = iter_next(&mut machine, iter);
        assert!(!done);
        assert_eq!(v2, Value::int32(30));

        let (done, _) = iter_next(&mut machine, iter);
        assert!(done);
    }
}
