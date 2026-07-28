use std::collections::BTreeMap;

use bamts_native::{Decoded, Value};

use super::{
    allocate_string, define_data, install_function, range_error, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.number_prototype();
    let constructor = install_function(heap, builtins, "Number", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert("Number".to_owned(), constructor);
    for (name, length, handler) in [
        ("isInteger", 1, is_integer::<H> as BuiltinHandler<H>),
        ("isSafeInteger", 1, is_safe_integer::<H>),
        ("isFinite", 1, is_finite::<H>),
        ("isNaN", 1, is_nan::<H>),
        ("parseFloat", 1, parse_float::<H>),
        ("parseInt", 2, parse_int::<H>),
    ] {
        let f = install_function(heap, builtins, name, length, handler);
        define_static(heap, constructor, name, f)
    }
    for (name, value) in [
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("EPSILON", f64::EPSILON),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", f64::from_bits(1)),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ] {
        define_static(heap, constructor, name, crate::number_value(value))
    }
    for (name, length, handler) in [
        ("toString", 1, to_string::<H> as BuiltinHandler<H>),
        ("toFixed", 1, to_fixed::<H>),
        ("valueOf", 0, value_of::<H>),
    ] {
        let f = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, f)
    }
}
fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("Number constructor must be native")
    };
    properties.insert(
        PropertyKey::Named(name.to_owned()),
        super::builtin_property(value),
    );
}
fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = if args.is_empty() {
        Value::int32(0)
    } else {
        machine.to_number(args[0])?
    };
    if constructing {
        Ok(BuiltinOutcome::Value(machine.box_primitive(value)?))
    } else {
        Ok(BuiltinOutcome::Value(value))
    }
}
fn primitive_number<H: Host>(
    machine: &Machine<'_, H>,
    this: Value,
    op: &'static str,
) -> Result<f64, EvalFailure> {
    let value = machine.unbox_primitive_or_self(this)?;
    match value.decode() {
        Some(Decoded::Number(n)) => Ok(n),
        Some(Decoded::Int32(n)) => Ok(f64::from(n as i32)),
        _ => Err(type_error(op)),
    }
}
fn numeric(value: Value) -> Option<f64> {
    match value.decode() {
        Some(Decoded::Number(n)) => Some(n),
        Some(Decoded::Int32(n)) => Some(f64::from(n as i32)),
        _ => None,
    }
}
fn is_integer<H: Host>(
    _: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let result = args
        .first()
        .and_then(|v| numeric(*v))
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0);
    Ok(BuiltinOutcome::Value(Value::boolean(result)))
}
fn is_safe_integer<H: Host>(
    _: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let result = args
        .first()
        .and_then(|v| numeric(*v))
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0);
    Ok(BuiltinOutcome::Value(Value::boolean(result)))
}
fn is_finite<H: Host>(
    _: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(Value::boolean(
        args.first()
            .and_then(|v| numeric(*v))
            .is_some_and(f64::is_finite),
    )))
}
fn is_nan<H: Host>(
    _: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(Value::boolean(
        args.first()
            .and_then(|v| numeric(*v))
            .is_some_and(f64::is_nan),
    )))
}
fn trim_js(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}
pub(super) fn parse_float<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let s = trim_js(&s);
    let value = if s.starts_with("Infinity") {
        f64::INFINITY
    } else if s.starts_with("-Infinity") {
        f64::NEG_INFINITY
    } else {
        let mut end = 0;
        for i in 1..=s.len() {
            if s[..i].parse::<f64>().is_ok() {
                end = i
            }
        }
        if end == 0 {
            f64::NAN
        } else {
            s[..end].parse().unwrap_or(f64::NAN)
        }
    };
    Ok(BuiltinOutcome::Value(crate::number_value(value)))
}
pub(super) fn parse_int<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let text = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let mut s = trim_js(&text);
    let mut sign = 1.0;
    if let Some(rest) = s.strip_prefix('-') {
        sign = -1.0;
        s = rest
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest
    }
    let requested =
        value_number(machine.to_number(args.get(1).copied().unwrap_or(Value::int32(0)))?) as i32;
    let mut radix = if requested == 0 { 10 } else { requested };
    if !(2..=36).contains(&radix) {
        return Ok(BuiltinOutcome::Value(crate::number_value(f64::NAN)));
    }
    if (requested == 0 || radix == 16) && s.get(..2).is_some_and(|x| x.eq_ignore_ascii_case("0x")) {
        radix = 16;
        s = &s[2..]
    }
    let mut value = 0.0;
    let mut found = false;
    for ch in s.chars() {
        let Some(d) = ch.to_digit(radix as u32) else {
            break;
        };
        found = true;
        value = value * radix as f64 + f64::from(d)
    }
    Ok(BuiltinOutcome::Value(crate::number_value(if found {
        sign * value
    } else {
        f64::NAN
    })))
}
pub(super) fn global_is_nan<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let n = value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    Ok(BuiltinOutcome::Value(Value::boolean(n.is_nan())))
}
pub(super) fn global_is_finite<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let n = value_number(machine.to_number(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    Ok(BuiltinOutcome::Value(Value::boolean(n.is_finite())))
}
fn to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let n = primitive_number(
        machine,
        this,
        "Number.prototype.toString requires that 'this' be a Number",
    )?;
    let radix =
        value_number(machine.to_number(args.first().copied().unwrap_or(Value::int32(10)))?) as u32;
    if !(2..=36).contains(&radix) {
        return Err(range_error(
            "toString() radix argument must be between 2 and 36",
        ));
    }
    let text = if radix == 10 || !n.is_finite() {
        crate::format_number(n)
    } else {
        radix_string(n, radix)
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}
fn radix_string(n: f64, radix: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let sign = if n.is_sign_negative() { "-" } else { "" };
    let mut integer = n.abs().trunc() as u128;
    if integer == 0 {
        return format!("{sign}0");
    }
    let mut out = Vec::new();
    while integer > 0 {
        out.push(DIGITS[(integer % u128::from(radix)) as usize] as char);
        integer /= u128::from(radix)
    }
    out.reverse();
    format!("{sign}{}", out.into_iter().collect::<String>())
}
fn to_fixed<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let n = primitive_number(
        machine,
        this,
        "Number.prototype.toFixed requires that 'this' be a Number",
    )?;
    let digits =
        value_number(machine.to_number(args.first().copied().unwrap_or(Value::int32(0)))?) as i32;
    if !(0..=100).contains(&digits) {
        return Err(range_error(
            "toFixed() digits argument must be between 0 and 100",
        ));
    }
    let text = if !n.is_finite() || n.abs() >= 1e21 {
        crate::format_number(n)
    } else {
        format!("{:.*}", digits as usize, n)
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}
fn value_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let n = primitive_number(
        machine,
        this,
        "Number.prototype.valueOf requires that 'this' be a Number",
    )?;
    Ok(BuiltinOutcome::Value(crate::number_value(n)))
}
