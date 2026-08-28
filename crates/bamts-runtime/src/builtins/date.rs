use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use super::{
    allocate_string, define_data, install_function, range_error, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    builtins.set_date_prototype(prototype);
    let constructor = install_function(heap, builtins, "Date", 7, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    let now = install_function(heap, builtins, "now", 0, now::<H>);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode("now")),
        super::builtin_property(now),
    );
    for (name, handler) in [
        ("getTime", get_time::<H> as BuiltinHandler<H>),
        ("valueOf", get_time::<H>),
        ("toISOString", to_iso_string::<H>),
    ] {
        let function = install_function(heap, builtins, name, 0, handler);
        define_data(heap, prototype, name, function);
    }
    globals.insert(EcmaString::encode("Date"), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        let text = to_date_string(time_clip(machine.host.now_ms() as f64))
            .unwrap_or_else(|| "Invalid Date".to_owned());
        return Ok(BuiltinOutcome::Value(allocate_string(
            machine,
            EcmaString::encode(&text),
        )?));
    }

    let milliseconds = if args.is_empty() {
        time_clip(machine.host.now_ms() as f64)
    } else if args.len() == 1 {
        let value = args[0];
        let copied_time = machine
            .runtime_slot(value)
            .map_err(EvalFailure::Runtime)?
            .and_then(|index| match &machine.heap[index] {
                HeapEntry::Date { time, .. } => Some(*time),
                _ => None,
            });
        if let Some(time) = copied_time {
            time_clip(time)
        } else {
            let primitive = machine.coerce_primitive_default(value)?;
            if let Some(text) = machine.string_value(primitive) {
                parse_iso_date(&text).unwrap_or(f64::NAN)
            } else {
                time_clip(value_number(machine.coerce_number_observable(primitive)?))
            }
        }
    } else {
        let mut components = [0.0; 7];
        components[2] = 1.0;
        for (i, component) in components.iter_mut().enumerate() {
            if let Some(&argument) = args.get(i) {
                *component = value_number(machine.coerce_number_observable(argument)?);
            }
        }
        date_from_components(components)
    };
    let default_prototype = machine.intrinsics.builtins.date_prototype();
    let new_target = machine.current_new_target();
    let prototype = if new_target != Value::UNDEFINED {
        let candidate = machine.get_named_property(new_target, "prototype")?;
        if machine.is_object(candidate) {
            candidate
        } else {
            default_prototype
        }
    } else {
        default_prototype
    };
    let object = machine
        .allocate(HeapEntry::Date {
            time: milliseconds,
            properties: PropertyMap::default(),
            prototype: Some(prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(object))
}

fn now<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(crate::number_value(
        machine.host.now_ms() as f64,
    )))
}

fn get_time<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    Ok(BuiltinOutcome::Value(crate::number_value(date_time(
        machine, this,
    )?)))
}

fn to_iso_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let milliseconds = date_time(machine, this)?;
    let text = iso_string(milliseconds).ok_or_else(|| range_error("Invalid time value"))?;
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        EcmaString::encode(&text),
    )?))
}

fn date_time<H: Host>(machine: &Machine<'_, H>, this: Value) -> Result<f64, EvalFailure> {
    let Some(index) = machine.runtime_slot(this).map_err(EvalFailure::Runtime)? else {
        return Err(type_error("Date method called on incompatible receiver"));
    };
    let HeapEntry::Date { time, .. } = &machine.heap[index] else {
        return Err(type_error("Date method called on incompatible receiver"));
    };
    Ok(*time)
}

fn iso_string(milliseconds: f64) -> Option<String> {
    if !milliseconds.is_finite() || milliseconds.abs() > 8_640_000_000_000_000.0 {
        return None;
    }
    let millis = milliseconds.trunc() as i64;
    let seconds = millis.div_euclid(1000);
    let millisecond = millis.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = day_seconds % 3600 / 60;
    let second = day_seconds % 60;
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{abs:06}", abs = year.unsigned_abs())
    } else {
        format!("+{year:06}")
    };
    Some(format!(
        "{year_text}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
    ))
}

/// `ToDateString`: the implementation-defined human-readable form used by both
/// `Date()` (no `new`) and `Date.prototype.toString`. The runtime is UTC-only,
/// so the offset is fixed at `GMT+0000` with the `(Coordinated Universal Time)`
/// time-zone name, matching the host's `now_ms` UTC epoch.
fn to_date_string(milliseconds: f64) -> Option<String> {
    if !milliseconds.is_finite() || milliseconds.abs() > 8_640_000_000_000_000.0 {
        return None;
    }
    let millis = milliseconds.trunc() as i64;
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = day_seconds % 3600 / 60;
    let second = day_seconds % 60;
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // 1970-01-01 was a Thursday (index 4); rem_euclid keeps negative days correct.
    let weekday = ((days.rem_euclid(7) + 4) % 7) as usize;
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{abs:06}", abs = year.unsigned_abs())
    } else {
        format!("+{year:06}")
    };
    Some(format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        WEEKDAYS[weekday],
        MONTHS[(month - 1) as usize],
        day,
        year_text,
        hour,
        minute,
        second
    ))
}

fn parse_iso_date(text: &EcmaString) -> Option<f64> {
    let units = text.as_units();
    let mut cursor = 0;
    let year = parse_year(units, &mut cursor)?;
    if cursor == units.len() {
        return time_clip_option(milliseconds_from_civil(year, 1, 1, 0, 0, 0, 0)?);
    }

    consume(units, &mut cursor, b'-')?;
    let month = parse_component(units, &mut cursor, 2)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if cursor == units.len() {
        return time_clip_option(milliseconds_from_civil(year, month, 1, 0, 0, 0, 0)?);
    }

    consume(units, &mut cursor, b'-')?;
    let day = parse_component(units, &mut cursor, 2)?;
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if cursor == units.len() {
        return time_clip_option(milliseconds_from_civil(year, month, day, 0, 0, 0, 0)?);
    }

    consume(units, &mut cursor, b'T')?;
    let hour = parse_component(units, &mut cursor, 2)?;
    consume(units, &mut cursor, b':')?;
    let minute = parse_component(units, &mut cursor, 2)?;
    let mut second = 0;
    let mut millisecond = 0;
    if matches!(units.get(cursor), Some(unit) if *unit == u16::from(b':')) {
        cursor += 1;
        second = parse_component(units, &mut cursor, 2)?;
        millisecond = parse_fraction(units, &mut cursor)?;
    }
    if hour > 24
        || minute > 59
        || second > 59
        || (hour == 24 && (minute != 0 || second != 0 || millisecond != 0))
    {
        return None;
    }

    let offset_minutes = match units.get(cursor).copied() {
        None => 0,
        Some(unit) if unit == u16::from(b'Z') => {
            cursor += 1;
            0
        }
        Some(unit @ (0x002B | 0x002D)) => {
            cursor += 1;
            let offset_hour = parse_component(units, &mut cursor, 2)?;
            consume(units, &mut cursor, b':')?;
            let offset_minute = parse_component(units, &mut cursor, 2)?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let minutes = offset_hour.checked_mul(60)?.checked_add(offset_minute)?;
            if unit == u16::from(b'-') {
                -minutes
            } else {
                minutes
            }
        }
        Some(_) => return None,
    };
    if cursor != units.len() {
        return None;
    }

    let milliseconds =
        milliseconds_from_civil(year, month, day, hour, minute, second, millisecond)?
            .checked_sub(offset_minutes.checked_mul(60_000)?)?;
    time_clip_option(milliseconds)
}

fn date_from_components(components: [f64; 7]) -> f64 {
    let [year, month, day, hour, minute, second, millisecond] = components;
    let Some(mut year) = integer_component(year) else {
        return f64::NAN;
    };
    let Some(month) = integer_component(month) else {
        return f64::NAN;
    };
    let Some(day) = integer_component(day) else {
        return f64::NAN;
    };
    let Some(hour) = integer_component(hour) else {
        return f64::NAN;
    };
    let Some(minute) = integer_component(minute) else {
        return f64::NAN;
    };
    let Some(second) = integer_component(second) else {
        return f64::NAN;
    };
    let Some(millisecond) = integer_component(millisecond) else {
        return f64::NAN;
    };

    if (0..=99).contains(&year) {
        year = year.saturating_add(1900);
    }
    let Some(year) = year.checked_add(month.div_euclid(12)) else {
        return f64::NAN;
    };
    let month = month.rem_euclid(12) + 1;
    let Some(milliseconds) =
        milliseconds_from_civil(year, month, day, hour, minute, second, millisecond)
    else {
        return f64::NAN;
    };
    time_clip(milliseconds as f64)
}

fn time_clip(milliseconds: f64) -> f64 {
    if !milliseconds.is_finite() || milliseconds.abs() > 8_640_000_000_000_000.0 {
        return f64::NAN;
    }
    let milliseconds = milliseconds.trunc();
    if milliseconds == 0.0 {
        0.0
    } else {
        milliseconds
    }
}

fn time_clip_option(milliseconds: i64) -> Option<f64> {
    let milliseconds = time_clip(milliseconds as f64);
    milliseconds.is_finite().then_some(milliseconds)
}

fn integer_component(value: f64) -> Option<i64> {
    (value.is_finite()
        && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value))
    .then(|| value.trunc() as i64)
}

fn parse_year(units: &[u16], cursor: &mut usize) -> Option<i64> {
    let signed = matches!(units.get(*cursor), Some(unit) if *unit == u16::from(b'+') || *unit == u16::from(b'-'));
    let sign = if signed {
        let sign = units[*cursor];
        *cursor += 1;
        sign
    } else {
        u16::from(b'+')
    };
    let year = parse_component(units, cursor, if signed { 6 } else { 4 })?;
    if sign == u16::from(b'-') {
        (year != 0).then_some(-year)
    } else {
        Some(year)
    }
}

fn parse_component(units: &[u16], cursor: &mut usize, len: usize) -> Option<i64> {
    let value = decimal_component(units, *cursor, len)?;
    *cursor = cursor.checked_add(len)?;
    Some(value)
}

fn consume(units: &[u16], cursor: &mut usize, expected: u8) -> Option<()> {
    (units.get(*cursor) == Some(&u16::from(expected))).then(|| {
        *cursor += 1;
    })
}

fn parse_fraction(units: &[u16], cursor: &mut usize) -> Option<i64> {
    if units.get(*cursor) != Some(&u16::from(b'.')) {
        return Some(0);
    }
    *cursor += 1;
    let mut millisecond = 0;
    let mut place = 100;
    let mut digits = 0;
    while let Some(&unit) = units.get(*cursor) {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&unit) {
            break;
        }
        if place > 0 {
            millisecond += i64::from(unit - u16::from(b'0')) * place;
            place /= 10;
        }
        digits += 1;
        *cursor += 1;
    }
    (digits > 0).then_some(millisecond)
}

fn milliseconds_from_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
) -> Option<i64> {
    let days = days_from_civil_checked(year, month, day)?;
    days.checked_mul(86_400_000)?
        .checked_add(hour.checked_mul(3_600_000)?)?
        .checked_add(minute.checked_mul(60_000)?)?
        .checked_add(second.checked_mul(1_000)?)?
        .checked_add(millisecond)
}

fn days_from_civil_checked(year: i64, month: i64, day: i64) -> Option<i64> {
    let year = year.checked_sub(i64::from(month <= 2))?;
    let era = year.div_euclid(400);
    let year_of_era = year.checked_sub(era.checked_mul(400)?)?;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day.checked_sub(1)?;
    era.checked_mul(146_097)?
        .checked_add(year_of_era * 365)?
        .checked_add(year_of_era / 4)?
        .checked_sub(year_of_era / 100)?
        .checked_add(day_of_year)?
        .checked_sub(719_468)
}

fn decimal_component(units: &[u16], start: usize, len: usize) -> Option<i64> {
    let end = start.checked_add(len)?;
    let mut value = 0_i64;
    for &unit in units.get(start..end)? {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&unit) {
            return None;
        }
        value = value * 10 + i64::from(unit - u16::from(b'0'));
    }
    Some(value)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use bamts_native::Value;

    use super::super::test_support::{blank_program, ordinary_object};
    use super::*;
    use crate::intrinsics::{BuiltinDef, BuiltinHandler, native_function};
    use crate::{Limits, Property};
    use bamts_bytecode::{FunctionId, ModuleId};

    struct TestHost;

    impl Host for TestHost {
        fn now_ms(&mut self) -> u64 {
            1_704_067_200_123
        }
    }

    fn call_date(machine: &mut Machine<'_, TestHost>, args: &[Value], constructing: bool) -> Value {
        let constructor = machine.intrinsics.global("Date").expect("Date exists");
        let index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: crate::NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("Date constructor is native");
        };
        let BuiltinOutcome::Value(value) = machine
            .call_builtin(id, Value::UNDEFINED, args, constructing)
            .expect("Date call succeeds")
        else {
            panic!("Date returns a value");
        };
        value
    }

    #[test]
    fn formats_node_24_iso_dates() {
        assert_eq!(iso_string(0.0).as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(
            iso_string(-1.0).as_deref(),
            Some("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(
            iso_string(1_704_067_200_123.0).as_deref(),
            Some("2024-01-01T00:00:00.123Z")
        );
    }

    #[test]
    fn call_form_ignores_every_argument_and_uses_host_time() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let garbage = machine
            .allocate(HeapEntry::String(EcmaString::encode("not a date")))
            .expect("string allocation succeeds");

        // Date() without `new` returns ToDateString — the human-readable
        // toString form — NOT the ISO-8601 string. ECMA-262 §21.4.2.1.
        let expected = to_date_string(1_704_067_200_123.0).unwrap();
        for args in [&[][..], &[Value::int32(0)][..], &[garbage][..]] {
            let value = call_date(&mut machine, args, false);
            assert_eq!(
                machine.string_value(value).unwrap().as_units(),
                EcmaString::encode(&expected).as_units(),
                "Date() must return the toString form, not ISO-8601"
            );
            // Guard against regression to the ISO form.
            assert_ne!(
                machine.string_value(value).unwrap().as_units(),
                EcmaString::encode("2024-01-01T00:00:00.123Z").as_units()
            );
        }
    }

    #[test]
    fn call_form_returns_to_date_string_not_iso() {
        // Direct regression: the spec ToDateString form starts with a weekday
        // and contains "GMT", never the ISO-8601 date/time separator.
        let text = to_date_string(1_704_067_200_123.0).unwrap();
        assert!(text.starts_with("Mon Jan 01 2024"));
        assert!(text.contains("GMT+0000 (Coordinated Universal Time)"));
        // ISO-8601 is `YYYY-MM-DDTHH:MM:SS.sssZ`; the ToDateString form is
        // `Www Mmm DD YYYY HH:MM:SS GMT+0000 (...)`. The two are told apart
        // by the `T` date/time separator at the fixed offset 10 — a `T` that
        // appears inside "Time" or weekday "Tue"/"Thu" lives elsewhere, so a
        // bare `contains('T')` cannot distinguish the forms.
        let bytes = text.as_bytes();
        let is_iso_prefix =
            bytes.len() >= 11 && bytes[4] == b'-' && bytes[7] == b'-' && bytes[10] == b'T';
        assert!(!is_iso_prefix, "ToDateString must not be ISO-8601");

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let value = call_date(&mut machine, &[], false);
        let result = machine.string_value(value).unwrap();
        assert_eq!(result.as_units(), EcmaString::encode(&text).as_units());
    }

    #[test]
    fn date_subclass_instance_has_subclass_prototype() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // Build a stand-in for `class D extends Date {}`: a constructor
        // function whose `prototype` is a fresh ordinary object that itself
        // inherits from Date.prototype.
        let date_constructor = machine.intrinsics.global("Date").expect("Date installed");
        let date_prototype = machine
            .get_named_property(date_constructor, "prototype")
            .unwrap();

        let sub_prototype = ordinary_object(&mut machine);
        machine
            .set_prototype(sub_prototype, date_prototype)
            .unwrap();

        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: sub_prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        let sub_constructor = machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .unwrap();

        // Construct via Date with new.target = sub_constructor, simulating
        // `super()` inside `class D extends Date`.
        let date_index = machine.runtime_slot(date_constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: crate::NativeCallable::Builtin(date_id),
            ..
        } = machine.heap[date_index]
        else {
            panic!("Date is a builtin");
        };
        let BuiltinOutcome::Value(instance) = machine
            .call_builtin_with_new_target(date_id, Value::UNDEFINED, &[], true, sub_constructor)
            .expect("Date construct succeeds")
        else {
            panic!("Date construct returns a value");
        };

        // The instance must inherit from the subclass prototype, not directly
        // from Date.prototype.
        assert_eq!(
            machine.internal_get_prototype_of(instance).unwrap(),
            Some(sub_prototype),
            "subclass instance must carry the subclass prototype"
        );
        assert!(
            machine.instance_of(instance, sub_constructor).unwrap(),
            "instanceof SubClass must be true"
        );
        assert!(
            machine.instance_of(instance, date_constructor).unwrap(),
            "instanceof Date must still be true"
        );
    }

    #[test]
    fn multi_argument_construction_normalizes_components_and_time_clips() {
        assert_eq!(
            date_from_components([2024.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]),
            1_704_067_200_000.0
        );
        assert_eq!(
            date_from_components([99.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]),
            915_148_800_000.0
        );
        assert_eq!(
            date_from_components([2024.0, 1.0, 30.0, 24.0, 0.0, 0.0, 0.0]),
            1_709_337_600_000.0
        );
        assert!(date_from_components([1.0e9, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]).is_nan());

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let value = call_date(
            &mut machine,
            &[Value::int32(2024), Value::int32(0), Value::int32(1)],
            true,
        );
        assert_eq!(date_time(&machine, value).unwrap(), 1_704_067_200_000.0);
    }

    #[test]
    fn parses_node_date_only_time_offset_and_extended_year_forms() {
        for (text, milliseconds) in [
            ("2024", 1_704_067_200_000.0),
            ("2024-01", 1_704_067_200_000.0),
            ("2024-01-01", 1_704_067_200_000.0),
            ("2024-01-01T00:00Z", 1_704_067_200_000.0),
            ("2024-01-01T00:00:00Z", 1_704_067_200_000.0),
            ("2024-01-01T00:00:00", 1_704_067_200_000.0),
            ("2024-01-01T00:00:00.1Z", 1_704_067_200_100.0),
            ("2024-01-01T01:30:00+01:30", 1_704_067_200_000.0),
            ("2024-01-01T00:00:00-02:30", 1_704_076_200_000.0),
            ("2024-01-01T24:00:00Z", 1_704_153_600_000.0),
        ] {
            assert_eq!(
                parse_iso_date(&EcmaString::encode(text)),
                Some(milliseconds),
                "{text}"
            );
        }

        for (text, expected) in [
            ("+006024-02-29T23:59:59.123Z", "6024-02-29T23:59:59.123Z"),
            ("+010000-01-01T00:00:00.000Z", "+010000-01-01T00:00:00.000Z"),
            ("-000001-01-01T00:00:00.000Z", "-000001-01-01T00:00:00.000Z"),
        ] {
            let milliseconds =
                parse_iso_date(&EcmaString::encode(text)).expect("valid extended year");
            assert_eq!(
                iso_string(milliseconds).as_deref(),
                Some(expected),
                "{text}"
            );
        }

        for text in [
            "2024-02-30T00:00:00.000Z",
            "2023-02-29T00:00:00.000Z",
            "2024-01-01T24:00:00.001Z",
            "2024-01-01T00:60:00.000Z",
            "2024-01-01T00:00:60.000Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01 00:00:00.000Z",
            "-000000-01-01T00:00:00.000Z",
        ] {
            assert_eq!(parse_iso_date(&EcmaString::encode(text)), None, "{text}");
        }
    }
    static VALUE_OF_CALLED: AtomicBool = AtomicBool::new(false);
    static DATE_VALUE_OF_CALLED: AtomicBool = AtomicBool::new(false);

    fn native(
        machine: &mut Machine<'_, TestHost>,
        name: &'static str,
        handler: BuiltinHandler<TestHost>,
    ) -> Value {
        let id = machine.intrinsics.builtins.register(BuiltinDef {
            name,
            length: 0,
            handler,
        });
        native_function(&mut machine.heap, id, name, 0)
    }

    fn generic_value_of(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        VALUE_OF_CALLED.store(true, Ordering::SeqCst);
        Ok(BuiltinOutcome::Value(Value::int32(12_345)))
    }

    fn date_value_of_override(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        DATE_VALUE_OF_CALLED.store(true, Ordering::SeqCst);
        Ok(BuiltinOutcome::Value(Value::int32(99_999)))
    }

    static CALL_ORDER: AtomicUsize = AtomicUsize::new(0);
    static YEAR_ORDER: AtomicUsize = AtomicUsize::new(0);
    static MONTH_ORDER: AtomicUsize = AtomicUsize::new(0);
    static DAY_ORDER: AtomicUsize = AtomicUsize::new(0);
    static TO_STRING_CALLED: AtomicBool = AtomicBool::new(false);

    fn year_value_of(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        YEAR_ORDER.store(
            CALL_ORDER.fetch_add(1, Ordering::SeqCst) + 1,
            Ordering::SeqCst,
        );
        Ok(BuiltinOutcome::Value(Value::int32(2024)))
    }

    fn month_value_of(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        MONTH_ORDER.store(
            CALL_ORDER.fetch_add(1, Ordering::SeqCst) + 1,
            Ordering::SeqCst,
        );
        Ok(BuiltinOutcome::Value(Value::int32(0)))
    }

    fn day_value_of(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        DAY_ORDER.store(
            CALL_ORDER.fetch_add(1, Ordering::SeqCst) + 1,
            Ordering::SeqCst,
        );
        Ok(BuiltinOutcome::Value(Value::int32(1)))
    }

    fn to_string_date_string(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        TO_STRING_CALLED.store(true, Ordering::SeqCst);
        Ok(BuiltinOutcome::Value(allocate_string(
            machine,
            EcmaString::encode("2024-01-01T00:00:00.000Z"),
        )?))
    }

    #[test]
    fn one_argument_copies_valid_and_invalid_date_time_values() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let valid = call_date(&mut machine, &[Value::int32(0)], true);
        let valid_copy = call_date(&mut machine, &[valid], true);
        assert_eq!(date_time(&machine, valid_copy).unwrap(), 0.0);

        let invalid = call_date(&mut machine, &[Value::number(f64::NAN)], true);
        assert!(date_time(&machine, invalid).unwrap().is_nan());
        let invalid_copy = call_date(&mut machine, &[invalid], true);
        assert!(date_time(&machine, invalid_copy).unwrap().is_nan());
    }

    #[test]
    fn one_argument_observes_generic_object_value_of() {
        VALUE_OF_CALLED.store(false, Ordering::SeqCst);
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = ordinary_object(&mut machine);
        let value_of = native(&mut machine, "valueOf", generic_value_of);
        machine
            .set_data_property(source, "valueOf", value_of)
            .expect("valueOf install succeeds");

        let copy = call_date(&mut machine, &[source], true);
        assert!(
            VALUE_OF_CALLED.load(Ordering::SeqCst),
            "valueOf must be called"
        );
        assert_eq!(date_time(&machine, copy).unwrap(), 12_345.0);
    }

    #[test]
    fn one_argument_copies_date_time_without_calling_overridden_value_of() {
        DATE_VALUE_OF_CALLED.store(false, Ordering::SeqCst);
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = call_date(&mut machine, &[Value::int32(0)], true);
        let value_of = native(&mut machine, "valueOf", date_value_of_override);
        machine
            .set_data_property(source, "valueOf", value_of)
            .expect("valueOf install succeeds");

        let copy = call_date(&mut machine, &[source], true);
        assert!(
            !DATE_VALUE_OF_CALLED.load(Ordering::SeqCst),
            "Date valueOf must not be called when copying"
        );
        assert_eq!(date_time(&machine, copy).unwrap(), 0.0);
    }

    #[test]
    fn one_argument_parses_object_to_string_primitive() {
        TO_STRING_CALLED.store(false, Ordering::SeqCst);
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let source = ordinary_object(&mut machine);
        let to_string = native(&mut machine, "toString", to_string_date_string);
        machine
            .set_data_property(source, "toString", to_string)
            .expect("toString install succeeds");

        let value = call_date(&mut machine, &[source], true);
        assert!(
            TO_STRING_CALLED.load(Ordering::SeqCst),
            "toString must be called"
        );
        assert_eq!(
            date_time(&machine, value).unwrap(),
            1_704_067_200_000.0,
            "object toString must be parsed as an ISO date string"
        );
    }

    #[test]
    fn multi_argument_components_call_value_of_in_order() {
        CALL_ORDER.store(0, Ordering::SeqCst);
        YEAR_ORDER.store(0, Ordering::SeqCst);
        MONTH_ORDER.store(0, Ordering::SeqCst);
        DAY_ORDER.store(0, Ordering::SeqCst);

        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let year = ordinary_object(&mut machine);
        let month = ordinary_object(&mut machine);
        let day = ordinary_object(&mut machine);

        let year_fn = native(&mut machine, "yearValueOf", year_value_of);
        let month_fn = native(&mut machine, "monthValueOf", month_value_of);
        let day_fn = native(&mut machine, "dayValueOf", day_value_of);

        machine
            .set_data_property(year, "valueOf", year_fn)
            .expect("year valueOf install succeeds");
        machine
            .set_data_property(month, "valueOf", month_fn)
            .expect("month valueOf install succeeds");
        machine
            .set_data_property(day, "valueOf", day_fn)
            .expect("day valueOf install succeeds");

        let value = call_date(&mut machine, &[year, month, day], true);
        assert_eq!(
            date_time(&machine, value).unwrap(),
            1_704_067_200_000.0,
            "multi-argument components must coerce to 2024-01-01"
        );
        assert_eq!(
            YEAR_ORDER.load(Ordering::SeqCst),
            1,
            "year must coerce first"
        );
        assert_eq!(
            MONTH_ORDER.load(Ordering::SeqCst),
            2,
            "month must coerce second"
        );
        assert_eq!(DAY_ORDER.load(Ordering::SeqCst), 3, "day must coerce third");
    }

    #[test]
    fn date_prototype_is_cached_not_global_lookup() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let ctor_before = machine
            .intrinsics
            .global("Date")
            .expect("Date global exists");
        let date_id = machine
            .intrinsics
            .builtins
            .id_named("Date")
            .expect("Date builtin id");

        // Delete and overwrite the global name. Construction through the saved
        // builtin id must still use the cached intrinsic prototype.
        machine
            .intrinsics
            .globals
            .remove(&EcmaString::encode("Date"));
        machine
            .intrinsics
            .globals
            .insert(EcmaString::encode("Date"), Value::int32(99));

        let BuiltinOutcome::Value(instance) = machine
            .call_builtin(date_id, Value::UNDEFINED, &[], true)
            .expect("Date construct succeeds")
        else {
            panic!("Date construct returns a value");
        };

        let index = machine
            .runtime_slot(instance)
            .expect("valid instance")
            .expect("slot");
        let HeapEntry::Date { prototype, .. } = &machine.heap[index] else {
            panic!("Date instance");
        };
        assert_eq!(
            *prototype,
            Some(machine.intrinsics.builtins.date_prototype())
        );

        // The cached prototype's "constructor" is still the original Date constructor.
        let proto_val = machine.intrinsics.builtins.date_prototype();
        let ctor = machine
            .get_named_property(proto_val, "constructor")
            .expect("prototype has constructor");
        assert_eq!(ctor, ctor_before);
    }
}
