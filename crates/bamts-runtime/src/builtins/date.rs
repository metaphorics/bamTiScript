use std::collections::BTreeMap;

use bamts_native::Value;

use super::{
    allocate_string, define_data, install_function, range_error, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

const DATE_VALUE: &str = "\0Date.value";

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
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
        PropertyKey::Named("now".to_owned()),
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
    globals.insert("Date".to_owned(), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let milliseconds = if let Some(value) = args.first().copied() {
        value_number(machine.to_number(value)?)
    } else {
        machine.host.now_ms() as f64
    };
    if !constructing {
        return Ok(BuiltinOutcome::Value(allocate_string(
            machine,
            iso_string(milliseconds).unwrap_or_else(|| "Invalid Date".to_owned()),
        )?));
    }
    let constructor = machine.intrinsics.global("Date").expect("Date installed");
    let prototype = machine.get_named_property(constructor, "prototype")?;
    let mut properties = PropertyMap::default();
    properties.insert(
        PropertyKey::Named(DATE_VALUE.to_owned()),
        Property::Data {
            value: crate::number_value(milliseconds),
            writable: true,
            enumerable: false,
            configurable: false,
        },
    );
    let object = machine
        .allocate(HeapEntry::Object {
            properties,
            prototype: Some(prototype),
            extensible: true,
            boxed_primitive: None,
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
    Ok(BuiltinOutcome::Value(date_value(machine, this)?))
}

fn to_iso_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let milliseconds = value_number(date_value(machine, this)?);
    let text = iso_string(milliseconds).ok_or_else(|| range_error("Invalid time value"))?;
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}

fn date_value<H: Host>(machine: &mut Machine<'_, H>, this: Value) -> Result<Value, EvalFailure> {
    let value = machine.get_named_property(this, DATE_VALUE)?;
    if value == Value::UNDEFINED {
        Err(type_error("Date method called on incompatible receiver"))
    } else {
        Ok(value)
    }
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
    use super::iso_string;
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
}
