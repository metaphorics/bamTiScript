use std::cmp::Ordering;
use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, to_integer_or_infinity,
    type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, IterationKind, Machine, PropertyKey};

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
    let HeapEntry::Array { properties, .. } = &mut heap[super::heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Symbol(super::heap_index(builtins.symbol_iterator()) as u32),
        super::builtin_property(values),
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
    elements(machine, this)?;
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
    elements(machine, this)?;
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
    elements(machine, this)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine,
        this,
        IterationKind::Entry,
    )?))
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
    use crate::{Limits, PropertyMap};

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

    /// Builds an object with a custom `Symbol.iterator` that yields `values`
    /// in order, bypassing any structural shortcuts.
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

    // ---- tests -------------------------------------------------------------

    #[test]
    fn array_from_observes_custom_iterator_override() {
        let module = module();
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
        machine.set_data_property(backing, "_values", values).unwrap();
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

        let module = module();
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
        let this_arg = object(&mut machine);
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
        let module = module();
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
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = object(&mut machine); // no Symbol.iterator
        let result = call_array(&mut machine, "from", &[source]);
        assert!(result.is_err());
    }
}
