use std::collections::BTreeMap;

use bamts_native::{Decoded, Value};

use super::{
    allocate_array, allocate_string, define_data, install_function, range_error,
    to_integer_or_infinity, type_error, value_number,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.string_prototype();
    let constructor = install_function(heap, builtins, "String", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert("String".to_owned(), constructor);
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
    ] {
        let f = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, f)
    }
}
fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("String constructor must be native")
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
    let text = if args.is_empty() {
        String::new()
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
fn text<H: Host>(machine: &Machine<'_, H>, this: Value) -> Result<String, EvalFailure> {
    if matches!(this.decode(), Some(Decoded::Undefined | Decoded::Null)) {
        return Err(type_error("String method called on null or undefined"));
    }
    machine.to_string(machine.unbox_primitive_or_self(this)?)
}
fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
fn from_units(v: &[u16]) -> String {
    String::from_utf16_lossy(v)
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
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        out.push(value_number(machine.to_number(*arg)?) as u16)
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        from_units(&out),
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
    let mut out = String::new();
    for (i, value) in values.iter().enumerate() {
        out.push_str(&machine.to_string(*value)?);
        if i + 1 < values.len() {
            out.push_str(&machine.to_string(args.get(i + 1).copied().unwrap_or(Value::UNDEFINED))?)
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(machine, out)?))
}
fn char_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = text(machine, this)?;
    let u = units(&s);
    let i = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    let out = if i < 0 || i as usize >= u.len() {
        String::new()
    } else {
        from_units(&u[i as usize..=i as usize])
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, out)?))
}
fn char_code_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let u = units(&text(machine, this)?);
    let i = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    Ok(BuiltinOutcome::Value(crate::number_value(
        if i < 0 || i as usize >= u.len() {
            f64::NAN
        } else {
            f64::from(u[i as usize])
        },
    )))
}
fn code_point_at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let u = units(&text(machine, this)?);
    let i = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?;
    if i < 0 || i as usize >= u.len() {
        return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
    }
    let first = u[i as usize];
    let cp = if (0xD800..=0xDBFF).contains(&first)
        && u.get(i as usize + 1)
            .is_some_and(|x| (0xDC00..=0xDFFF).contains(x))
    {
        0x10000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(u[i as usize + 1]) - 0xDC00)
    } else {
        u32::from(first)
    };
    Ok(BuiltinOutcome::Value(crate::number_value(cp as f64)))
}
fn at<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = text(machine, this)?;
    let u = units(&s);
    let mut i = integer(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if i < 0 {
        i += u.len() as isize
    }
    if i < 0 || i as usize >= u.len() {
        Ok(BuiltinOutcome::Value(Value::UNDEFINED))
    } else {
        Ok(BuiltinOutcome::Value(allocate_string(
            machine,
            from_units(&u[i as usize..=i as usize]),
        )?))
    }
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
    let u = units(&text(machine, this)?);
    let a = rel(
        integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?,
        u.len(),
    );
    let b = rel(
        integer(
            machine,
            args.get(1)
                .copied()
                .unwrap_or(crate::number_value(u.len() as f64)),
        )?,
        u.len(),
    );
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        if b > a {
            from_units(&u[a..b])
        } else {
            String::new()
        },
    )?))
}
fn substring<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let u = units(&text(machine, this)?);
    let mut a = integer(machine, args.first().copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    a = a.min(u.len());
    let mut b = if args.len() < 2 || args[1] == Value::UNDEFINED {
        u.len()
    } else {
        integer(machine, args[1])?.max(0) as usize
    };
    b = b.min(u.len());
    if a > b {
        std::mem::swap(&mut a, &mut b)
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        from_units(&u[a..b]),
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
    let h = units(&text(machine, this)?);
    let n = units(&machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let start = integer(machine, args.get(1).copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    Ok(BuiltinOutcome::Value(crate::number_value(
        search_units(&h, &n, start).map_or(-1.0, |i| i as f64),
    )))
}
fn last_index_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let h = units(&text(machine, this)?);
    let n = units(&machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let end = if args.len() < 2 || args[1] == Value::UNDEFINED {
        h.len()
    } else {
        integer(machine, args[1])?.max(0) as usize
    }
    .min(h.len());
    let found = (0..=end)
        .rev()
        .find(|i| h.get(*i..i.saturating_add(n.len())).is_some_and(|w| w == n));
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
    let h = units(&text(machine, this)?);
    let n = units(&machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let p = integer(machine, args.get(1).copied().unwrap_or(Value::int32(0)))?.max(0) as usize;
    Ok(BuiltinOutcome::Value(Value::boolean(
        h.get(p..p + n.len()).is_some_and(|w| w == n),
    )))
}
fn ends_with<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let h = units(&text(machine, this)?);
    let n = units(&machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?);
    let end = if args.len() < 2 || args[1] == Value::UNDEFINED {
        h.len()
    } else {
        integer(machine, args[1])?.max(0) as usize
    }
    .min(h.len());
    Ok(BuiltinOutcome::Value(Value::boolean(
        end >= n.len() && h[end - n.len()..end] == n,
    )))
}
fn reject_regexp<H: Host>(machine: &Machine<'_, H>, value: Value) -> Result<(), EvalFailure> {
    let is_regexp = machine
        .runtime_slot(value)
        .map_err(EvalFailure::Runtime)?
        .is_some_and(|index| matches!(machine.heap[index], HeapEntry::RegExp { .. }));
    if is_regexp {
        Err(type_error(
            "regular expression string methods require RegExp support",
        ))
    } else {
        Ok(())
    }
}

fn split<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(separator) = args.first().copied() {
        reject_regexp(machine, separator)?;
    }
    let s = text(machine, this)?;
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
    let parts: Vec<String> = if args.is_empty() || args[0] == Value::UNDEFINED {
        vec![s]
    } else {
        let sep = machine.to_string(args[0])?;
        if sep.is_empty() {
            units(&s).into_iter().map(|u| from_units(&[u])).collect()
        } else {
            s.split(&sep).map(str::to_owned).collect()
        }
    };
    let mut out = Vec::new();
    for p in parts.into_iter().take(limit) {
        out.push(allocate_string(machine, p)?)
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, out)?))
}
fn replacement<H: Host>(
    machine: &mut Machine<'_, H>,
    replacer: Value,
    matched: &str,
    index: usize,
    whole: &str,
) -> Result<String, EvalFailure> {
    if machine.is_callable(replacer)? {
        let m = allocate_string(machine, matched.to_owned())?;
        let w = allocate_string(machine, whole.to_owned())?;
        return machine
            .call_value(
                replacer,
                Value::UNDEFINED,
                &[m, crate::number_value(index as f64), w],
            )
            .and_then(|v| machine.to_string(v));
    }
    let r = machine.to_string(replacer)?;
    Ok(r.replace("$$", "\0")
        .replace("$&", matched)
        .replace("$`", &whole[..index])
        .replace("$'", &whole[index + matched.len()..])
        .replace('\0', "$"))
}
fn replace_impl<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    all: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(search) = args.first().copied() {
        reject_regexp(machine, search)?;
    }
    let s = text(machine, this)?;
    let needle = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let replacer = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let mut out = String::new();
    let mut cursor = 0;
    let matches: Vec<usize> = if all {
        s.match_indices(&needle).map(|(i, _)| i).collect()
    } else {
        s.find(&needle).into_iter().collect()
    };
    for i in matches {
        if i < cursor {
            continue;
        }
        out.push_str(&s[cursor..i]);
        out.push_str(&replacement(machine, replacer, &needle, i, &s)?);
        cursor = i + needle.len();
        if needle.is_empty() && cursor < s.len() {
            let ch = s[cursor..].chars().next().expect("nonempty");
            out.push(ch);
            cursor += ch.len_utf8()
        }
    }
    out.push_str(&s[cursor..]);
    Ok(BuiltinOutcome::Value(allocate_string(machine, out)?))
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
macro_rules! trim_fn {
    ($n:ident,$m:ident) => {
        fn $n<H: Host>(
            machine: &mut Machine<'_, H>,
            this: Value,
            _: &[Value],
            _: bool,
        ) -> Result<BuiltinOutcome, EvalFailure> {
            let s = text(machine, this)?;
            Ok(BuiltinOutcome::Value(allocate_string(
                machine,
                s.$m().to_owned(),
            )?))
        }
    };
}
trim_fn!(trim, trim);
trim_fn!(trim_start, trim_start);
trim_fn!(trim_end, trim_end);
fn to_upper<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = text(machine, this)?.to_uppercase();
    Ok(BuiltinOutcome::Value(allocate_string(machine, s)?))
}
fn to_lower<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = text(machine, this)?.to_lowercase();
    Ok(BuiltinOutcome::Value(allocate_string(machine, s)?))
}
fn pad<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    start: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = text(machine, this)?;
    let len = units(&s).len();
    let target = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?
        .max(0.0) as usize;
    if target <= len {
        return Ok(BuiltinOutcome::Value(allocate_string(machine, s)?));
    }
    let filler = if args.len() < 2 || args[1] == Value::UNDEFINED {
        " ".to_owned()
    } else {
        machine.to_string(args[1])?
    };
    if filler.is_empty() {
        return Ok(BuiltinOutcome::Value(allocate_string(machine, s)?));
    }
    let f = units(&filler);
    let need = target - len;
    let p: Vec<u16> = f.iter().copied().cycle().take(need).collect();
    let out = if start {
        format!("{}{}", from_units(&p), s)
    } else {
        format!("{}{}", s, from_units(&p))
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, out)?))
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
    let s = text(machine, this)?;
    let n = to_integer_or_infinity(machine, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if n < 0.0 || n.is_infinite() {
        return Err(range_error("Invalid count value"));
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        s.repeat(n as usize),
    )?))
}
fn concat<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let mut s = text(machine, this)?;
    for a in args {
        s.push_str(&machine.to_string(*a)?)
    }
    Ok(BuiltinOutcome::Value(allocate_string(machine, s)?))
}
fn normalize<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if let Some(form) = args.first().filter(|v| **v != Value::UNDEFINED) {
        let f = machine.to_string(*form)?;
        if !matches!(f.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
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
    let s = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let mut out = String::new();
    for b in s.bytes() {
        if uri_unescaped(b) {
            out.push(char::from(b))
        } else {
            out.push_str(&format!("%{b:02X}"))
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(machine, out)?))
}
pub(super) fn decode_uri_component<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let s = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(type_error("URI malformed"));
            }
            let h = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .ok_or_else(|| type_error("URI malformed"))?;
            out.push(h);
            i += 3
        } else {
            out.push(bytes[i]);
            i += 1
        }
    }
    let text = String::from_utf8(out).map_err(|_| type_error("URI malformed"))?;
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}
