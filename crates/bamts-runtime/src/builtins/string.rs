use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
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
    if let Some(search) = args.first().copied()
        && super::regexp::regexp_parts(machine, search).is_some()
    {
        return replace_regexp(machine, this, args, all);
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

fn string_iterator<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let values = text(machine, this)?
        .chars()
        .map(|character| allocate_string(machine, character.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
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
        Ok((super::regexp::compile(machine, &pattern, "")?, None))
    }
}

fn string_match<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input_text = text(machine, this)?;
    let input = EcmaString::from_utf8(&input_text);
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
            super::regexp::slice_units(&input, matched.range).to_utf8_lossy(),
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
    let input_text = text(machine, this)?;
    let input = EcmaString::from_utf8(&input_text);
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
    let input_text = text(machine, this)?;
    let input = EcmaString::from_utf8(&input_text);
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
    let input_text = text(machine, this)?;
    let input = EcmaString::from_utf8(&input_text);
    let separator = args[0];
    let (pattern, flags) =
        super::regexp::regexp_parts(machine, separator).expect("caller checked RegExp argument");
    let regex = super::regexp::compile(machine, &pattern, &flags.replace('y', ""))?;
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
        values.push(allocate_string(machine, piece.to_utf8_lossy())?);
    }
    Ok(BuiltinOutcome::Value(allocate_array(machine, values)?))
}

fn replace_regexp<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    replace_all_call: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input_text = text(machine, this)?;
    let input = EcmaString::from_utf8(&input_text);
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
    let mut output = String::new();
    let mut cursor = 0;
    for matched in matches {
        output.push_str(
            &super::regexp::slice_units(&input, cursor..matched.range.start).to_utf8_lossy(),
        );
        output.push_str(&regexp_replacement(machine, replacer, &input, &matched)?);
        cursor = matched.range.end;
        if !regex.flags().global {
            break;
        }
    }
    output.push_str(&super::regexp::slice_units(&input, cursor..input.len_units()).to_utf8_lossy());
    Ok(BuiltinOutcome::Value(allocate_string(machine, output)?))
}

fn regexp_replacement<H: Host>(
    machine: &mut Machine<'_, H>,
    replacer: Value,
    input: &EcmaString,
    matched: &crate::intrinsics::regexp::Match,
) -> Result<String, EvalFailure> {
    let matched_text = super::regexp::slice_units(input, matched.range.clone()).to_utf8_lossy();
    if machine.is_callable(replacer)? {
        let mut arguments = Vec::with_capacity(matched.captures.len() + 2);
        for capture in &matched.captures {
            arguments.push(match capture {
                Some(range) => allocate_string(
                    machine,
                    super::regexp::slice_units(input, range.clone()).to_utf8_lossy(),
                )?,
                None => Value::UNDEFINED,
            });
        }
        arguments.push(crate::number_value(matched.range.start as f64));
        arguments.push(allocate_string(machine, input.to_utf8_lossy())?);
        return machine
            .call_value(replacer, Value::UNDEFINED, &arguments)
            .and_then(|value| machine.to_string(value));
    }
    let replacement = machine.to_string(replacer)?;
    let before = super::regexp::slice_units(input, 0..matched.range.start).to_utf8_lossy();
    let after =
        super::regexp::slice_units(input, matched.range.end..input.len_units()).to_utf8_lossy();
    let mut output = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        let Some(next) = chars.peek().copied() else {
            output.push('$');
            break;
        };
        match next {
            '$' => output.push('$'),
            '&' => output.push_str(&matched_text),
            '`' => output.push_str(&before),
            '\'' => output.push_str(&after),
            digit if digit.is_ascii_digit() && digit != '0' => {
                let capture = digit.to_digit(10).expect("digit") as usize;
                if let Some(Some(range)) = matched.captures.get(capture) {
                    output.push_str(
                        &super::regexp::slice_units(input, range.clone()).to_utf8_lossy(),
                    );
                }
            }
            other => {
                output.push('$');
                output.push(other);
            }
        }
        chars.next();
    }
    Ok(output)
}
