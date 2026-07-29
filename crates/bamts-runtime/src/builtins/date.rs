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
    let constructor = install_function(heap, builtins, "Date", 7, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    let now = install_function(heap, builtins, "now", 0, now::<H>);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8("now")),
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
    globals.insert(EcmaString::from_utf8("Date"), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let milliseconds = if args.len() == 1 {
        let value = args[0];
        match machine.string_value(value) {
            Some(text) => parse_iso_date(&text).unwrap_or(f64::NAN),
            None => value_number(machine.to_number(value)?),
        }
    } else if let Some(value) = args.first().copied() {
        value_number(machine.to_number(value)?)
    } else {
        machine.host.now_ms() as f64
    };
    if !constructing {
        let text = iso_string(milliseconds).unwrap_or_else(|| "Invalid Date".to_owned());
        return Ok(BuiltinOutcome::Value(allocate_string(
            machine,
            EcmaString::from_utf8(&text),
        )?));
    }
    let constructor = machine.intrinsics.global("Date").expect("Date installed");
    let prototype = machine.get_named_property(constructor, "prototype")?;
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
        EcmaString::from_utf8(&text),
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

fn parse_iso_date(text: &EcmaString) -> Option<f64> {
    let units = text.as_units();
    if units.len() != 24
        || units[4] != u16::from(b'-')
        || units[7] != u16::from(b'-')
        || units[10] != u16::from(b'T')
        || units[13] != u16::from(b':')
        || units[16] != u16::from(b':')
        || units[19] != u16::from(b'.')
        || units[23] != u16::from(b'Z')
    {
        return None;
    }

    let year = decimal_component(units, 0, 4)?;
    let month = decimal_component(units, 5, 2)?;
    let day = decimal_component(units, 8, 2)?;
    let hour = decimal_component(units, 11, 2)?;
    let minute = decimal_component(units, 14, 2)?;
    let second = decimal_component(units, 17, 2)?;
    let millisecond = decimal_component(units, 20, 3)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(
        (days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millisecond)
            as f64,
    )
}

fn decimal_component(units: &[u16], start: usize, len: usize) -> Option<i64> {
    let mut value = 0_i64;
    for &unit in &units[start..start + len] {
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

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
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
    use super::{iso_string, parse_iso_date};
    use bamts_bytecode::EcmaString;

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
    fn parses_pinned_utc_iso_dates_without_utf8_flattening() {
        for (text, milliseconds) in [
            ("1970-01-01T00:00:00.000Z", 0.0),
            ("2024-02-29T23:59:59.123Z", 1_709_251_199_123.0),
            ("2026-01-01T00:00:00.000Z", 1_767_225_600_000.0),
        ] {
            assert_eq!(
                parse_iso_date(&EcmaString::from_utf8(text)),
                Some(milliseconds),
                "{text}"
            );
        }

        for text in [
            "2024-02-30T00:00:00.000Z",
            "2023-02-29T00:00:00.000Z",
            "2024-01-01T24:00:00.000Z",
            "2024-01-01T00:60:00.000Z",
            "2024-01-01T00:00:60.000Z",
            "2024-01-01T00:00:00.000+00:00",
            "2024-01-01T00:00:00.00Z",
            "2024-01-01 00:00:00.000Z",
        ] {
            assert_eq!(parse_iso_date(&EcmaString::from_utf8(text)), None, "{text}");
        }
    }
}
