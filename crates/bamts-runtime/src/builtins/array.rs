use std::cmp::Ordering;
use std::collections::BTreeMap;

use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, to_integer_or_infinity,
    type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.array_prototype();
    let constructor = install_function(heap, builtins, "Array", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert("Array".to_owned(), constructor);
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
}

fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("Array constructor must be native")
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
    let mut elements = if let Some(values) = machine.array_elements(source)? {
        values
    } else if let Some(text) = machine.string_value(source) {
        text.encode_utf16()
            .map(|unit| allocate_string(machine, String::from_utf16_lossy(&[unit])))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err(type_error("Array.from requires an array-like object"));
    };
    if let Some(callback) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
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
    let sep = if args.is_empty() || args[0] == Value::UNDEFINED {
        ",".to_owned()
    } else {
        machine.to_string(args[0])?
    };
    let mut parts = Vec::with_capacity(values.len());
    for value in values {
        parts.push(
            if value == Value::HOLE
                || matches!(value.decode(), Some(Decoded::Undefined | Decoded::Null))
            {
                String::new()
            } else {
                machine.to_string(value)?
            },
        );
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        parts.join(&sep),
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
                (Ok(a), Ok(b)) => a.encode_utf16().cmp(b.encode_utf16()),
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
