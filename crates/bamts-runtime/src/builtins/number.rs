use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use super::{
    allocate_string, define_data, install_function, range_error, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.number_prototype();
    let constructor = install_function(heap, builtins, "Number", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::from_utf8("Number"), constructor);
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
fn is_js_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000a | 0x000b | 0x000c | 0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000
            ..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
    )
}
fn trim_start_js(units: &[u16]) -> &[u16] {
    &units[units
        .iter()
        .take_while(|unit| is_js_whitespace(**unit))
        .count()..]
}
fn is_ascii_digit(unit: u16) -> bool {
    (u16::from(b'0')..=u16::from(b'9')).contains(&unit)
}
fn starts_ascii(units: &[u16], ascii: &[u8]) -> bool {
    units.len() >= ascii.len()
        && units
            .iter()
            .zip(ascii)
            .all(|(unit, byte)| *unit == u16::from(*byte))
}
fn parse_float_units(units: &[u16]) -> f64 {
    let units = trim_start_js(units);
    let mut cursor = 0;
    let mut sign = 1.0;
    match units.first() {
        Some(unit) if *unit == u16::from(b'-') => {
            sign = -1.0;
            cursor = 1;
        }
        Some(unit) if *unit == u16::from(b'+') => cursor = 1,
        _ => {}
    }
    if starts_ascii(&units[cursor..], b"Infinity") {
        return sign * f64::INFINITY;
    }
    let digits_start = cursor;
    while units.get(cursor).is_some_and(|unit| is_ascii_digit(*unit)) {
        cursor += 1;
    }
    let mut found = cursor > digits_start;
    if units.get(cursor) == Some(&u16::from(b'.')) {
        cursor += 1;
        let fraction_start = cursor;
        while units.get(cursor).is_some_and(|unit| is_ascii_digit(*unit)) {
            cursor += 1;
        }
        found |= cursor > fraction_start;
    }
    if !found {
        return f64::NAN;
    }
    if matches!(units.get(cursor), Some(unit) if *unit == u16::from(b'e') || *unit == u16::from(b'E'))
    {
        let exponent = cursor;
        cursor += 1;
        if matches!(units.get(cursor), Some(unit) if *unit == u16::from(b'+') || *unit == u16::from(b'-'))
        {
            cursor += 1;
        }
        let exponent_digits = cursor;
        while units.get(cursor).is_some_and(|unit| is_ascii_digit(*unit)) {
            cursor += 1;
        }
        if cursor == exponent_digits {
            cursor = exponent;
        }
    }
    EcmaString::from_units(&units[..cursor])
        .to_utf8_strict()
        .expect("numeric prefix contains only ASCII units")
        .parse()
        .unwrap_or(f64::NAN)
}
pub(super) fn parse_float<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let text = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(crate::number_value(
        parse_float_units(text.as_units()),
    )))
}
/// ECMAScript ToInt32 applied to the parseInt radix argument: wraps the
/// number modulo 2^32 into the signed 32-bit range. `value_number(...) as
/// i32` saturates in Rust, so radices like 2^32 and 2^32 + 10 must be
/// converted here instead of cast.
fn to_int32_radix(number: f64) -> i32 {
    if !number.is_finite() || number == 0.0 {
        0
    } else {
        number.trunc().rem_euclid(4_294_967_296.0) as i32
    }
}
pub(super) fn parse_int<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let text = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let units = trim_start_js(text.as_units());
    let mut cursor = 0;
    let mut sign = 1.0;
    match units.first() {
        Some(unit) if *unit == u16::from(b'-') => {
            sign = -1.0;
            cursor = 1;
        }
        Some(unit) if *unit == u16::from(b'+') => cursor = 1,
        _ => {}
    }
    let requested = to_int32_radix(value_number(
        machine.to_number(args.get(1).copied().unwrap_or(Value::int32(0)))?,
    ));
    let mut radix = if requested == 0 { 10 } else { requested };
    if !(2..=36).contains(&radix) {
        return Ok(BuiltinOutcome::Value(crate::number_value(f64::NAN)));
    }
    if (requested == 0 || radix == 16)
        && (starts_ascii(&units[cursor..], b"0x") || starts_ascii(&units[cursor..], b"0X"))
    {
        radix = 16;
        cursor += 2;
    }
    let mut value = 0.0;
    let mut found = false;
    for &unit in &units[cursor..] {
        let digit = match unit {
            unit if (u16::from(b'0')..=u16::from(b'9')).contains(&unit) => {
                u32::from(unit - u16::from(b'0'))
            }
            unit if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) => {
                u32::from(unit - u16::from(b'a')) + 10
            }
            unit if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) => {
                u32::from(unit - u16::from(b'A')) + 10
            }
            _ => break,
        };
        if digit >= radix as u32 {
            break;
        }
        found = true;
        value = value * f64::from(radix) + f64::from(digit);
        // Once the accumulator overflows to infinity, further digits cannot
        // change the result; stop scanning to avoid charging the machine for
        // arbitrarily long trailing input.
        if !value.is_finite() {
            break;
        }
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
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::from_utf8(&text),
    )?))
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
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::from_utf8(&text),
    )?))
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{TestHost, blank_program};
    use super::*;
    use crate::Limits;

    #[test]
    fn numeric_prefixes_stop_at_lone_surrogates() {
        let program = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let text = machine
            .allocate(HeapEntry::String(EcmaString::from_units(&[
                u16::from(b'1'),
                0xd800,
            ])))
            .expect("malformed UTF-16 string allocation succeeds");

        let BuiltinOutcome::Value(float) =
            parse_float(&mut machine, Value::UNDEFINED, &[text], false)
                .expect("parseFloat handles a lone surrogate after its prefix")
        else {
            panic!("parseFloat returns a value");
        };
        let BuiltinOutcome::Value(integer) = parse_int(
            &mut machine,
            Value::UNDEFINED,
            &[text, Value::int32(10)],
            false,
        )
        .expect("parseInt handles a lone surrogate after its prefix") else {
            panic!("parseInt returns a value");
        };

        assert_eq!(float, Value::int32(1));
        assert_eq!(integer, Value::int32(1));
    }

    fn parse_int_radix(machine: &mut Machine<'_, TestHost>, input: &str, radix: Value) -> Value {
        let text = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8(input)))
            .expect("input string allocation succeeds");
        let BuiltinOutcome::Value(result) =
            parse_int(&mut *machine, Value::UNDEFINED, &[text, radix], false)
                .expect("parseInt returns a value")
        else {
            panic!("parseInt returns a value");
        };
        result
    }

    #[test]
    fn parse_int_wraps_radix_through_to_int32() {
        let program = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());

        // ToInt32 wraps modulo 2^32: 2^32 -> 0 (defaults to 10), 2^32 + 10 -> 10.
        assert_eq!(
            parse_int_radix(&mut machine, "10", Value::number(4_294_967_296.0)),
            Value::int32(10),
            "radix 2^32 must wrap to 0 and default to base 10"
        );
        assert_eq!(
            parse_int_radix(&mut machine, "10", Value::number(4_294_967_306.0)),
            Value::int32(10),
            "radix 2^32 + 10 must wrap to 10"
        );
        // 2^32 + 2 wraps to 2, so "10" parses as binary.
        assert_eq!(
            parse_int_radix(&mut machine, "10", Value::number(4_294_967_298.0)),
            Value::int32(2),
            "radix 2^32 + 2 must wrap to 2"
        );
        // A non-integer radix is truncated by ToInt32: 10.5 -> 10.
        assert_eq!(
            parse_int_radix(&mut machine, "10", Value::number(10.5)),
            Value::int32(10),
            "non-integer radix 10.5 must truncate to 10"
        );
        // 2^32 - 1 wraps to -1, which is outside [2, 36], so NaN results.
        assert!(
            value_number(parse_int_radix(
                &mut machine,
                "10",
                Value::number(4_294_967_295.0)
            ))
            .is_nan(),
            "radix 2^32 - 1 must wrap to -1 and yield NaN"
        );
    }

    #[test]
    fn parse_int_stops_accumulating_once_the_value_overflows() {
        let program = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let input = "9".repeat(500);
        let result = parse_int_radix(&mut machine, &input, Value::int32(10));
        assert_eq!(
            value_number(result),
            f64::INFINITY,
            "an overflowing integer literal must saturate to Infinity"
        );
    }
}
