use std::collections::BTreeMap;
use std::ops::Range;

use bamts_native::Value;

use super::{
    allocate_array, allocate_string, builtin_property, define_data, install_function, type_error,
};
use crate::intrinsics::regexp::{Match, Regex};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let constructor = install_function(heap, builtins, "RegExp", 2, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("exec", 1, exec::<H> as BuiltinHandler<H>),
        ("test", 1, test::<H>),
        ("toString", 0, to_string::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
        globals.insert(format!("\0RegExp.{name}"), function);
    }
    globals.insert("\0RegExp.prototype".to_owned(), prototype);
    globals.insert("RegExp".to_owned(), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (pattern, inherited_flags) = args.first().copied().map_or_else(
        || Ok((String::new(), String::new())),
        |value| {
            if let Some(parts) = regexp_parts(machine, value) {
                Ok(parts)
            } else {
                Ok((machine.to_string(value)?, String::new()))
            }
        },
    )?;
    let flags = if let Some(value) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        machine.to_string(value)?
    } else {
        inherited_flags
    };
    compile(machine, &pattern, &flags)?;
    let mut properties = PropertyMap::default();
    for name in ["exec", "test", "toString"] {
        let value = machine
            .intrinsics
            .global(&format!("\0RegExp.{name}"))
            .expect("RegExp method installed");
        properties.insert(PropertyKey::Named(name.to_owned()), builtin_property(value));
    }
    for (name, value, writable) in [
        (
            "source",
            allocate_string(
                machine,
                if pattern.is_empty() {
                    "(?:)".to_owned()
                } else {
                    pattern.clone()
                },
            )?,
            false,
        ),
        (
            "flags",
            allocate_string(
                machine,
                Regex::compile(&pattern, &flags)
                    .expect("validated")
                    .flags()
                    .canonical(),
            )?,
            false,
        ),
        ("lastIndex", Value::int32(0), true),
    ] {
        properties.insert(
            PropertyKey::Named(name.to_owned()),
            Property::Data {
                value,
                writable,
                enumerable: false,
                configurable: false,
            },
        );
    }
    let value = machine
        .allocate(HeapEntry::RegExp {
            pattern,
            flags,
            properties,
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(value))
}

pub(super) fn compile<H: Host>(
    machine: &mut Machine<'_, H>,
    pattern: &str,
    flags: &str,
) -> Result<Regex, EvalFailure> {
    Regex::compile(pattern, flags).map_err(|error| {
        let id = machine
            .intrinsics
            .builtins
            .id_named("SyntaxError")
            .expect("SyntaxError installed");
        machine.throw_error(id, error.message().to_owned())
    })
}

pub(super) fn regexp_parts<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Option<(String, String)> {
    let index = machine.runtime_slot(value).ok().flatten()?;
    match &machine.heap[index] {
        HeapEntry::RegExp { pattern, flags, .. } => Some((pattern.clone(), flags.clone())),
        _ => None,
    }
}

pub(super) fn execute<H: Host>(
    machine: &mut Machine<'_, H>,
    regexp: Value,
    input: &str,
) -> Result<Option<Match>, EvalFailure> {
    let (pattern, flags) = regexp_parts(machine, regexp)
        .ok_or_else(|| type_error("RegExp method called on incompatible receiver"))?;
    let regex = compile(machine, &pattern, &flags)?;
    let uses_last_index = regex.flags().global || regex.flags().sticky;
    let start = if uses_last_index {
        index_value(machine.get_named_property(regexp, "lastIndex")?)
    } else {
        0
    };
    let matched = regex.exec(input, start);
    if uses_last_index {
        let next = matched.as_ref().map_or(0, |value| value.range.end);
        machine.set_data_property(regexp, "lastIndex", crate::number_value(next as f64))?;
    }
    Ok(matched)
}

fn exec<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let Some(matched) = execute(machine, this, &input)? else {
        return Ok(BuiltinOutcome::Value(Value::NULL));
    };
    Ok(BuiltinOutcome::Value(match_array(
        machine, &input, matched,
    )?))
}

fn test<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        execute(machine, this, &input)?.is_some(),
    )))
}

fn to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (source, flags) = regexp_parts(machine, this)
        .ok_or_else(|| type_error("RegExp method called on incompatible receiver"))?;
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        format!("/{}/{flags}", source.replace('/', "\\/")),
    )?))
}

pub(super) fn match_array<H: Host>(
    machine: &mut Machine<'_, H>,
    input: &str,
    matched: Match,
) -> Result<Value, EvalFailure> {
    let mut values = Vec::with_capacity(matched.captures.len());
    for capture in &matched.captures {
        values.push(match capture {
            Some(range) => allocate_string(machine, slice_chars(input, range.clone()))?,
            None => Value::UNDEFINED,
        });
    }
    let array = allocate_array(machine, values)?;
    machine.set_data_property(
        array,
        "index",
        crate::number_value(matched.range.start as f64),
    )?;
    let input_value = allocate_string(machine, input.to_owned())?;
    machine.set_data_property(array, "input", input_value)?;
    if matched.named.is_empty() {
        machine.set_data_property(array, "groups", Value::UNDEFINED)?;
    } else {
        let groups = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: None,
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        for (name, range) in matched.named {
            let value = match range {
                Some(range) => allocate_string(machine, slice_chars(input, range))?,
                None => Value::UNDEFINED,
            };
            machine.set_data_property(groups, &name, value)?;
        }
        machine.set_data_property(array, "groups", groups)?;
    }
    Ok(array)
}

pub(super) fn slice_chars(input: &str, range: Range<usize>) -> String {
    input
        .chars()
        .skip(range.start)
        .take(range.end - range.start)
        .collect()
}
fn index_value(value: Value) -> usize {
    match value.decode() {
        Some(bamts_native::Decoded::Int32(value)) => value as usize,
        Some(bamts_native::Decoded::Number(value)) if value.is_finite() && value > 0.0 => {
            value as usize
        }
        _ => 0,
    }
}
