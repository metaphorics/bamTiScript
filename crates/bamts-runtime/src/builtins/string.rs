use std::collections::BTreeMap;

use bamts_bytecode::{EcmaString, EcmaStringBuilder};
use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, range_error,
    to_integer_or_infinity, type_error, uri_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.string_prototype();
    let constructor = install_function(heap, builtins, "String", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::from_utf8("String"), constructor);
    for (name, length, handler) in [
        ("fromCharCode", 1, from_char_code::<H> as BuiltinHandler<H>),
        ("raw", 1, raw::<H>),
    ] {
        let f = install_function(heap, builtins, name, length, handler);
        define_static(heap, constructor, name, f)
    }
    for (name, length, handler) in [
        ("charAt", 1, char_at::<H> as BuiltinHandler<H>),
        ("charCodeAt", 1, char_code_at::<H>),
        ("codePointAt", 1, code_point_at::<H>),
        ("at", 1, at::<H>),
        ("slice", 2, slice::<H>),
        ("substring", 2, substring::<H>),
        ("indexOf", 1, index_of::<H>),
        ("lastIndexOf", 1, last_index_of::<H>),
        ("includes", 1, includes::<H>),
        ("startsWith", 1, starts_with::<H>),
        ("endsWith", 1, ends_with::<H>),
        ("split", 2, split::<H>),
        ("replace", 2, replace::<H>),
        ("replaceAll", 2, replace_all::<H>),
        ("trim", 0, trim::<H>),
        ("trimStart", 0, trim_start::<H>),
        ("trimEnd", 0, trim_end::<H>),
        ("toUpperCase", 0, to_upper::<H>),
        ("toLowerCase", 0, to_lower::<H>),
        ("padStart", 1, pad_start::<H>),
        ("padEnd", 1, pad_end::<H>),
        ("repeat", 1, repeat::<H>),
        ("concat", 1, concat::<H>),
        ("normalize", 0, normalize::<H>),
        ("match", 1, string_match::<H>),
        ("matchAll", 1, match_all::<H>),
        ("search", 1, search::<H>),
    ] {
        let f = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, f)
    }
    let iterator = install_function(heap, builtins, "[Symbol.iterator]", 0, string_iterator::<H>);
    let HeapEntry::Object { properties, .. } = &mut heap[super::heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Symbol(super::heap_index(builtins.symbol_iterator()) as u32),
        super::builtin_property(iterator),
    );
}
fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("String constructor must be native")
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        super::builtin_property(value),
    );
}
fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let text = if args.is_empty() {
        EcmaString::default()
    } else {
        machine.to_string(args[0])?
    };
    let value = allocate_string(machine, text)?;
    if constructing {
        Ok(BuiltinOutcome::Value(machine.box_primitive(value)?))
    } else {
        Ok(BuiltinOutcome::Value(value))
    }
}
fn text<H: Host>(machine: &Machine<'_, H>, this: Value) -> Result<EcmaString, EvalFailure> {
    if matches!(this.decode(), Some(Decoded::Undefined | Decoded::Null)) {
        return Err(type_error("String method called on null or undefined"));
    }
    machine.to_string(machine.unbox_primitive_or_self(this)?)
}
fn append(out: &mut EcmaStringBuilder, text: &EcmaString) {
    for &unit in text.as_units() {
        out.push_unit(unit);
    }
}
fn integer<H: Host>(machine: &Machine<'_, H>, value: Value) -> Result<isize, EvalFailure> {
    Ok(to_integer_or_infinity(machine, value)? as isize)
}
fn from_char_code<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut out = EcmaStringBuilder::with_capacity(args.len());
    for arg in args {
        out.push_unit(value_number(machine.to_number(*arg)?) as u16);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        out.finish(),
    )?))
}
fn raw<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let template = args.first().copied().unwrap_or(Value::UNDEFINED);
    let raw = machine.get_named_property(template, "raw")?;
    let values = machine
        .array_elements(raw)?
        .ok_or_else(|| type_error("String.raw requires template.raw"))?;
    let mut out = EcmaStringBuilder::new();
    for (i, value) in values.iter().enumerate() {
        append(&mut out, &machine.to_string(*value)?);
        if i + 1 < values.len() {
            append(
                &mut out,
                &machine.to_string(args.get(i + 1).copied().unwrap_or(Value::UNDEFINED))?,
            );
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        out.finish(),
    )?))
}
fn char_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let index = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    let result = if index < 0 {
        EcmaString::default()
    } else {
        string
            .unit_at(index as usize)
            .map_or_else(EcmaString::default, |unit| EcmaString::from_units(&[unit]))
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, result)?))
}
fn char_code_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let index = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    let result = if index < 0 {
        f64::NAN
    } else {
        string.unit_at(index as usize).map_or(f64::NAN, f64::from)
    };
    Ok(BuiltinOutcome::Value(crate::number_value(result)))
}
fn code_point_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let index = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    if index < 0 || index as usize >= string.len_units() {
        return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
    }
    let offset = index as usize;
    let code_point = string
        .code_points()
        .find_map(|(candidate, code_point)| (candidate == offset).then_some(code_point))
        .unwrap_or_else(|| u32::from(string.unit_at(offset).expect("offset is in bounds")));
    Ok(BuiltinOutcome::Value(crate::number_value(
        code_point as f64,
    )))
}
fn at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let mut index = integer(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if index < 0 {
        index += string.len_units() as isize;
    }
    let Some(unit) = (index >= 0)
        .then(|| string.unit_at(index as usize))
        .flatten()
    else {
        return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
    };
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::from_units(&[unit]),
    )?))
}
fn rel(i: isize, len: usize) -> usize {
    if i < 0 {
        (len as isize + i).max(0) as usize
    } else {
        (i as usize).min(len)
    }
}
fn slice<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let start = rel(
        integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?,
        string.len_units(),
    );
    let end = rel(
        integer(
            machine,
            args.get(1)
                .copied()
                .unwrap_or(crate::number_value(string.len_units() as f64)),
        )?,
        string.len_units(),
    );
    let result = if end > start {
        string.slice_units(start..end)
    } else {
        EcmaString::default()
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, result)?))
}
fn substring<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let mut start =
        integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    start = start.min(string.len_units());
    let mut end = if args.len() < 2 || args[1] == Value::UNDEFINED {
        string.len_units()
    } else {
        integer(machine, args[1])?.max(0) as usize
    };
    end = end.min(string.len_units());
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        string.slice_units(start..end),
    )?))
}
fn search_units(h: &[u16], n: &[u16], start: usize) -> Option<usize> {
    if n.is_empty() {
        return Some(start.min(h.len()));
    }
    h.get(start..)?
        .windows(n.len())
        .position(|w| w == n)
        .map(|i| i + start)
}
fn index_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let haystack = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let start = integer(machine, args.get(1).copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    Ok(BuiltinOutcome::Value(crate::number_value(
        search_units(haystack.as_units(), needle.as_units(), start).map_or(-1.0, |i| i as f64),
    )))
}
fn last_index_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let haystack = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let end = if args.len() < 2 || args[1] == Value::UNDEFINED {
        haystack.len_units()
    } else {
        integer(machine, args[1])?.max(0) as usize
    }
    .min(haystack.len_units());
    let found = (0..=end).rev().find(|index| {
        haystack
            .as_units()
            .get(*index..index.saturating_add(needle.len_units()))
            .is_some_and(|window| window == needle.as_units())
    });
    Ok(BuiltinOutcome::Value(crate::number_value(
        found.map_or(-1.0, |i| i as f64),
    )))
}
fn includes<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let BuiltinOutcome::Value(v) = index_of(machine, this, args, false)? else {
        unreachable!()
    };
    Ok(BuiltinOutcome::Value(Value::boolean(
        value_number(v) >= 0.0,
    )))
}
fn starts_with<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let haystack = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let position =
        integer(machine, args.get(1).copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    Ok(BuiltinOutcome::Value(Value::boolean(
        haystack
            .as_units()
            .get(position..position.saturating_add(needle.len_units()))
            .is_some_and(|window| window == needle.as_units()),
    )))
}
fn ends_with<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let haystack = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let end = if args.len() < 2 || args[1] == Value::UNDEFINED {
        haystack.len_units()
    } else {
        integer(machine, args[1])?.max(0) as usize
    }
    .min(haystack.len_units());
    Ok(BuiltinOutcome::Value(Value::boolean(
        end >= needle.len_units()
            && haystack.as_units()[end - needle.len_units()..end] == *needle.as_units(),
    )))
}

fn split<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(separator) = args.first().copied()
        && super::regexp::regexp_parts(machine, separator).is_some()
    {
        return split_regexp(machine, this, args);
    }
    let string = text(machine, this)?;
    let limit = value_number(
        machine.to_number(
            args.get(1)
                .copied()
                .unwrap_or(crate::number_value(u32::MAX as f64)),
        )?,
    ) as u32 as usize;
    if limit == 0 {
        return Ok(BuiltinOutcome::Value(allocate_array(machine, Vec::new())?));
    }
    let parts = if args.is_empty() || args[0] == Value::UNDEFINED {
        vec![string]
    } else {
        let separator = machine.to_string(args[0])?;
        if separator.is_empty() {
            string
                .as_units()
                .iter()
                .map(|unit| EcmaString::from_units(&[*unit]))
                .collect()
        } else {
            let mut parts = Vec::new();
            let mut cursor = 0;
            while let Some(offset) = search_units(string.as_units(), separator.as_units(), cursor) {
                parts.push(string.slice_units(cursor..offset));
                cursor = offset + separator.len_units();
            }
            parts.push(string.slice_units(cursor..string.len_units()));
            parts
        }
    };
    let mut values = Vec::new();
    for part in parts.into_iter().take(limit) {
        values.push(allocate_string(machine, part)?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
}
fn replacement<H: Host>(
    machine: &mut Machine<'_, H>,
    replacer: Value,
    matched: &EcmaString,
    index: usize,
    whole: &EcmaString,
) -> Result<EcmaString, EvalFailure> {
    if machine.is_callable(replacer)? {
        let matched_value = allocate_string(machine, matched.clone())?;
        let whole_value = allocate_string(machine, whole.clone())?;
        return machine
            .call_value(
                replacer,
                Value::UNDEFINED,
                &[
                    matched_value,
                    crate::number_value(index as f64),
                    whole_value,
                ],
            )
            .and_then(|value| machine.to_string(value));
    }
    let template = machine.to_string(replacer)?;
    let mut output = EcmaStringBuilder::new();
    let units = template.as_units();
    let mut offset = 0;
    while offset < units.len() {
        if units[offset] != u16::from(b'$') || offset + 1 == units.len() {
            output.push_unit(units[offset]);
            offset += 1;
            continue;
        }
        match units[offset + 1] {
            unit if unit == u16::from(b'$') => output.push_unit(u16::from(b'$')),
            unit if unit == u16::from(b'&') => append(&mut output, matched),
            unit if unit == u16::from(b'`') => {
                append(&mut output, &whole.slice_units(0..index));
            }
            unit if unit == u16::from(b'\'') => {
                append(
                    &mut output,
                    &whole.slice_units(index + matched.len_units()..whole.len_units()),
                );
            }
            _ => {
                output.push_unit(units[offset]);
                offset += 1;
                continue;
            }
        }
        offset += 2;
    }
    Ok(output.finish())
}
fn replace_impl<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    all: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(search) = args.first().copied()
        && super::regexp::regexp_parts(machine, search).is_some()
    {
        return replace_regexp(machine, this, args, all);
    }
    let string = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let replacer = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let mut output = EcmaStringBuilder::new();
    let mut cursor = 0;
    while cursor <= string.len_units() {
        let Some(index) = search_units(string.as_units(), needle.as_units(), cursor) else {
            break;
        };
        append(&mut output, &string.slice_units(cursor..index));
        append(
            &mut output,
            &replacement(machine, replacer, &needle, index, &string)?,
        );
        cursor = index + needle.len_units();
        if !all {
            break;
        }
        if needle.is_empty() && cursor < string.len_units() {
            output.push_unit(string.unit_at(cursor).expect("cursor is in bounds"));
            cursor += 1;
        } else if needle.is_empty() && cursor == string.len_units() {
            break;
        }
    }
    append(
        &mut output,
        &string.slice_units(cursor.min(string.len_units())..string.len_units()),
    );
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn replace<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    replace_impl(machine, this, args, false)
}
fn replace_all<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    replace_impl(machine, this, args, true)
}
fn is_js_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000D
            | 0x0020
            | 0x00A0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    )
}

fn trim_range(text: &EcmaString, trim_start: bool, trim_end: bool) -> EcmaString {
    let units = text.as_units();
    let start = if trim_start {
        units
            .iter()
            .position(|unit| !is_js_whitespace(*unit))
            .unwrap_or(units.len())
    } else {
        0
    };
    let end = if trim_end {
        units
            .iter()
            .rposition(|unit| !is_js_whitespace(*unit))
            .map_or(start, |index| index + 1)
    } else {
        units.len()
    };
    text.slice_units(start..end.max(start))
}

macro_rules! trim_fn {
    ($name:ident, $start:literal, $end:literal) => {
        fn $name<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _: &[Value],
            _: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            let string = text(machine, this)?;
            Ok(BuiltinOutcome::Value(allocate_string(
                machine,
                trim_range(&string, $start, $end),
            )?))
        }
    };
}
trim_fn!(trim, true, true);
trim_fn!(trim_start, true, false);
trim_fn!(trim_end, false, true);
fn to_upper<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = text(machine, this)?;
    let mut output = EcmaStringBuilder::new();
    for (_, code_point) in source.code_points() {
        if let Some(character) = char::from_u32(code_point) {
            for mapped in character.to_uppercase() {
                output
                    .push_code_point(mapped as u32)
                    .expect("Rust char is valid");
            }
        } else {
            output.push_unit(code_point as u16);
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn to_lower<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = text(machine, this)?;
    let mut output = EcmaStringBuilder::new();
    for (_, code_point) in source.code_points() {
        if let Some(character) = char::from_u32(code_point) {
            for mapped in character.to_lowercase() {
                output
                    .push_code_point(mapped as u32)
                    .expect("Rust char is valid");
            }
        } else {
            output.push_unit(code_point as u16);
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn pad<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    start: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let target = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?
        .max(0.0) as usize;
    if target <= string.len_units() {
        return Ok(BuiltinOutcome::Value(allocate_string(machine, string)?));
    }
    let filler = if args.len() < 2 || args[1] == Value::UNDEFINED {
        EcmaString::from_utf8(" ")
    } else {
        machine.to_string(args[1])?
    };
    if filler.is_empty() {
        return Ok(BuiltinOutcome::Value(allocate_string(machine, string)?));
    }
    let needed = target - string.len_units();
    let padding = EcmaString::from_units(
        &filler
            .as_units()
            .iter()
            .copied()
            .cycle()
            .take(needed)
            .collect::<Vec<_>>(),
    );
    let mut output = EcmaStringBuilder::with_capacity(target);
    if start {
        append(&mut output, &padding);
        append(&mut output, &string);
    } else {
        append(&mut output, &string);
        append(&mut output, &padding);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn pad_start<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    pad(machine, this, args, true)
}
fn pad_end<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    pad(machine, this, args, false)
}
fn repeat<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let string = text(machine, this)?;
    let count = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if count < 0.0 || count.is_infinite() {
        return Err(range_error("Invalid count value"));
    }
    let mut output =
        EcmaStringBuilder::with_capacity(string.len_units().saturating_mul(count as usize));
    for _ in 0..count as usize {
        append(&mut output, &string);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn concat<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut output = EcmaStringBuilder::new();
    append(&mut output, &text(machine, this)?);
    for argument in args {
        append(&mut output, &machine.to_string(*argument)?);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}
fn normalize<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(form) = args.first().filter(|value| **value != Value::UNDEFINED) {
        let form = machine.to_string(*form)?;
        if !form.eq_ascii("NFC")
            && !form.eq_ascii("NFD")
            && !form.eq_ascii("NFKC")
            && !form.eq_ascii("NFKD")
        {
            return Err(range_error(
                "The normalization form should be one of NFC, NFD, NFKC, NFKD",
            ));
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        text(machine, this)?,
    )?))
}
fn uri_unescaped(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte)
}
pub(super) fn encode_uri_component<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = machine
        .to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?
        .to_utf8_strict()
        .map_err(|_| uri_error("URI malformed"))?;
    let mut output = String::new();
    for byte in source.bytes() {
        if uri_unescaped(byte) {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::from_utf8(&output),
    )?))
}
fn hex_value(unit: u16) -> Option<u8> {
    match unit {
        0x30..=0x39 => Some((unit - 0x30) as u8),
        0x41..=0x46 => Some((unit - 0x41 + 10) as u8),
        0x61..=0x66 => Some((unit - 0x61 + 10) as u8),
        _ => None,
    }
}

fn percent_octet(units: &[u16], offset: usize) -> Option<u8> {
    if units.get(offset) != Some(&u16::from(b'%')) {
        return None;
    }
    let high = hex_value(*units.get(offset + 1)?)?;
    let low = hex_value(*units.get(offset + 2)?)?;
    Some((high << 4) | low)
}

fn utf8_sequence_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

pub(super) fn decode_uri_component<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let units = source.as_units();
    let mut output = EcmaStringBuilder::with_capacity(units.len());
    let mut offset = 0;
    while offset < units.len() {
        if units[offset] != u16::from(b'%') {
            output.push_unit(units[offset]);
            offset += 1;
            continue;
        }

        let first = percent_octet(units, offset).ok_or_else(|| uri_error("URI malformed"))?;
        let sequence_len = utf8_sequence_len(first).ok_or_else(|| uri_error("URI malformed"))?;
        if sequence_len == 1 {
            output.push_unit(u16::from(first));
            offset += 3;
            continue;
        }

        let mut octets = [0; 4];
        octets[0] = first;
        for octet in &mut octets[1..sequence_len] {
            offset += 3;
            *octet = percent_octet(units, offset).ok_or_else(|| uri_error("URI malformed"))?;
        }
        let decoded =
            std::str::from_utf8(&octets[..sequence_len]).map_err(|_| uri_error("URI malformed"))?;
        output.push_utf8(decoded);
        offset += 3;
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}

fn string_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let pieces: Vec<EcmaString> = text(machine, this)?
        .code_points()
        .map(|(_, code_point)| {
            let mut builder = EcmaStringBuilder::new();
            builder
                .push_code_point(code_point)
                .expect("EcmaString code point is valid");
            builder.finish()
        })
        .collect();
    let mut values = Vec::with_capacity(pieces.len());
    for piece in pieces {
        values.push(allocate_string(machine, piece)?);
    }
    let source = allocate_array(machine, values)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine, source,
    )?))
}

fn regexp_for_argument<H: Host>(
    machine: &mut Machine<'_, H>,
    value: Value,
) -> Result<(crate::intrinsics::regexp::Regex, Option<Value>), EvalFailure> {
    if let Some((pattern, flags)) = super::regexp::regexp_parts(machine, value) {
        Ok((
            super::regexp::compile(machine, &pattern, &flags)?,
            Some(value),
        ))
    } else {
        let pattern = machine.to_string(value)?;
        Ok((
            super::regexp::compile(machine, &pattern, &EcmaString::default())?,
            None,
        ))
    }
}

fn string_match<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = text(machine, this)?;
    let argument = args.first().copied().unwrap_or(Value::UNDEFINED);
    let (regex, object) = regexp_for_argument(machine, argument)?;
    if !regex.flags().global {
        let matched = match object {
            Some(regexp) => super::regexp::execute(machine, regexp, &input)?,
            None => regex.exec(&input, 0),
        };
        return match matched {
            Some(matched) => Ok(BuiltinOutcome::Value(super::regexp::match_array(
                machine, &input, matched,
            )?)),
            None => Ok(BuiltinOutcome::Value(Value::NULL)),
        };
    }
    if let Some(regexp) = object {
        machine.set_data_property(regexp, "lastIndex", Value::int32(0))?;
    }
    let matches = collect_matches(&regex, &input);
    if matches.is_empty() {
        return Ok(BuiltinOutcome::Value(Value::NULL));
    }
    let mut values = Vec::with_capacity(matches.len());
    for matched in matches {
        values.push(allocate_string(
            machine,
            super::regexp::slice_units(&input, matched.range),
        )?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
}

fn match_all<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = text(machine, this)?;
    let argument = args.first().copied().unwrap_or(Value::UNDEFINED);
    let (regex, object) = regexp_for_argument(machine, argument)?;
    if object.is_some() && !regex.flags().global {
        return Err(type_error(
            "String.prototype.matchAll requires a global RegExp",
        ));
    }
    let mut values = Vec::new();
    for matched in collect_matches(&regex, &input) {
        values.push(super::regexp::match_array(machine, &input, matched)?);
    }
    let source = allocate_array(machine, values)?;
    Ok(BuiltinOutcome::Value(super::collections::iterator(
        machine, source,
    )?))
}

fn search<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = text(machine, this)?;
    let argument = args.first().copied().unwrap_or(Value::UNDEFINED);
    let (regex, _) = regexp_for_argument(machine, argument)?;
    Ok(BuiltinOutcome::Value(crate::number_value(
        regex
            .exec(&input, 0)
            .map_or(-1.0, |matched| matched.range.start as f64),
    )))
}

fn collect_matches(
    regex: &crate::intrinsics::regexp::Regex,
    input: &EcmaString,
) -> Vec<crate::intrinsics::regexp::Match> {
    let mut matches = Vec::new();
    let mut start = 0;
    let length = input.len_units();
    while start <= length {
        let Some(matched) = regex.exec(input, start) else {
            break;
        };
        let next = if matched.range.end == matched.range.start && matched.range.end < length {
            matched.range.end
                + crate::intrinsics::regexp::next_code_point(
                    input.as_units(),
                    matched.range.end,
                    regex.flags().unicode,
                )
                .1
        } else {
            matched.range.end + usize::from(matched.range.end == matched.range.start)
        };
        matches.push(matched);
        if !regex.flags().global || next > length {
            break;
        }
        start = next;
    }
    matches
}

fn split_regexp<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = text(machine, this)?;
    let separator = args[0];
    let (pattern, flags) =
        super::regexp::regexp_parts(machine, separator).expect("caller checked RegExp argument");
    let stickyless_flags = EcmaString::from_units(
        &flags
            .as_units()
            .iter()
            .copied()
            .filter(|unit| *unit != u16::from(b'y'))
            .collect::<Vec<_>>(),
    );
    let regex = super::regexp::compile(machine, &pattern, &stickyless_flags)?;
    let limit = value_number(
        machine.to_number(
            args.get(1)
                .copied()
                .unwrap_or(crate::number_value(u32::MAX as f64)),
        )?,
    ) as u32 as usize;
    let mut pieces = Vec::new();
    let mut cursor = 0;
    let length = input.len_units();
    while cursor <= length && pieces.len() < limit {
        let Some(matched) = regex.exec(&input, cursor) else {
            break;
        };
        pieces.push(super::regexp::slice_units(
            &input,
            cursor..matched.range.start,
        ));
        for capture in matched.captures.iter().skip(1) {
            if pieces.len() == limit {
                break;
            }
            pieces.push(capture.clone().map_or_else(EcmaString::default, |range| {
                super::regexp::slice_units(&input, range)
            }));
        }
        cursor = if matched.range.end == matched.range.start && matched.range.end < length {
            matched.range.end
                + crate::intrinsics::regexp::next_code_point(
                    input.as_units(),
                    matched.range.end,
                    regex.flags().unicode,
                )
                .1
        } else {
            matched.range.end + usize::from(matched.range.end == matched.range.start)
        };
    }
    if pieces.len() < limit {
        pieces.push(super::regexp::slice_units(
            &input,
            cursor.min(length)..length,
        ));
    }
    let mut values = Vec::new();
    for piece in pieces.into_iter().take(limit) {
        values.push(allocate_string(machine, piece)?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
}

fn replace_regexp<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    replace_all_call: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = text(machine, this)?;
    let regexp = args[0];
    let (pattern, flags) =
        super::regexp::regexp_parts(machine, regexp).expect("caller checked RegExp argument");
    let regex = super::regexp::compile(machine, &pattern, &flags)?;
    if replace_all_call && !regex.flags().global {
        return Err(type_error(
            "String.prototype.replaceAll requires a global RegExp",
        ));
    }
    let replacer = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let matches = collect_matches(&regex, &input);
    let mut output = EcmaStringBuilder::new();
    let mut cursor = 0;
    for matched in matches {
        append(
            &mut output,
            &super::regexp::slice_units(&input, cursor..matched.range.start),
        );
        append(
            &mut output,
            &regexp_replacement(machine, replacer, &input, &matched)?,
        );
        cursor = matched.range.end;
        if !regex.flags().global {
            break;
        }
    }
    append(
        &mut output,
        &super::regexp::slice_units(&input, cursor..input.len_units()),
    );
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}

fn regexp_replacement<H: Host>(
    machine: &mut Machine<'_, H>,
    replacer: Value,
    input: &EcmaString,
    matched: &crate::intrinsics::regexp::Match,
) -> Result<EcmaString, EvalFailure> {
    let matched_text = super::regexp::slice_units(input, matched.range.clone());
    if machine.is_callable(replacer)? {
        let mut arguments = Vec::with_capacity(matched.captures.len() + 2);
        for capture in &matched.captures {
            arguments.push(match capture {
                Some(range) => {
                    allocate_string(machine, super::regexp::slice_units(input, range.clone()))?
                }
                None => Value::UNDEFINED,
            });
        }
        arguments.push(crate::number_value(matched.range.start as f64));
        arguments.push(allocate_string(machine, input.clone())?);
        return machine
            .call_value(replacer, Value::UNDEFINED, &arguments)
            .and_then(|value| machine.to_string(value));
    }
    let replacement = machine.to_string(replacer)?;
    let before = super::regexp::slice_units(input, 0..matched.range.start);
    let after = super::regexp::slice_units(input, matched.range.end..input.len_units());
    let mut output = EcmaStringBuilder::new();
    let units = replacement.as_units();
    let mut offset = 0;
    while offset < units.len() {
        if units[offset] != u16::from(b'$') || offset + 1 == units.len() {
            output.push_unit(units[offset]);
            offset += 1;
            continue;
        }
        let next = units[offset + 1];
        if next == u16::from(b'$') {
            output.push_unit(u16::from(b'$'));
        } else if next == u16::from(b'&') {
            append(&mut output, &matched_text);
        } else if next == u16::from(b'`') {
            append(&mut output, &before);
        } else if next == u16::from(b'\'') {
            append(&mut output, &after);
        } else if (u16::from(b'1')..=u16::from(b'9')).contains(&next) {
            let capture = usize::from(next - u16::from(b'0'));
            if let Some(Some(range)) = matched.captures.get(capture) {
                append(
                    &mut output,
                    &super::regexp::slice_units(input, range.clone()),
                );
            }
        } else {
            output.push_unit(units[offset]);
            offset += 1;
            continue;
        }
        offset += 2;
    }
    Ok(output.finish())
}
