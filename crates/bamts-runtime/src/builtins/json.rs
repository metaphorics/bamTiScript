use std::collections::{BTreeMap, BTreeSet};

use bamts_native::{Decoded, Value};

use super::{allocate_array, allocate_string, define_data, install_function, type_error};
use crate::intrinsics::{BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<String, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let json = super::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(builtins.object_prototype()),
            extensible: true,
            boxed_primitive: None,
        },
    );
    let parse = install_function(heap, builtins, "parse", 2, parse::<H>);
    let stringify = install_function(heap, builtins, "stringify", 3, stringify::<H>);
    define_data(heap, json, "parse", parse);
    define_data(heap, json, "stringify", stringify);
    globals.insert("JSON".to_owned(), json);
}

fn parse<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let value = match Parser::new(&source).parse(machine) {
        Ok(value) => value,
        Err(message) => {
            let id = machine
                .intrinsics
                .builtins
                .id_named("SyntaxError")
                .expect("SyntaxError is installed");
            return Err(machine.throw_error(id, message));
        }
    };
    if let Some(reviver) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        let root = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        machine.set_data_property(root, "", value)?;
        let value = walk_reviver(machine, root, "", reviver)?;
        return Ok(BuiltinOutcome::Value(value));
    }
    Ok(BuiltinOutcome::Value(value))
}
fn walk_reviver<H: Host>(
    machine: &mut Machine<'_, H>,
    holder: Value,
    key: &str,
    reviver: Value,
) -> Result<Value, EvalFailure> {
    let value = machine.get_named_property(holder, key)?;
    if let Some(elements) = machine.array_elements(value)? {
        for i in 0..elements.len() {
            let name = i.to_string();
            let child = walk_reviver(machine, value, &name, reviver)?;
            if child == Value::UNDEFINED {
                machine.delete_named_property(value, &name)?;
            } else {
                machine.set_data_property(value, &name, child)?;
            }
        }
    } else if machine.is_object(value) {
        for name in machine.enumerable_keys(value)? {
            let child = walk_reviver(machine, value, &name, reviver)?;
            if child == Value::UNDEFINED {
                machine.delete_named_property(value, &name)?;
            } else {
                machine.set_data_property(value, &name, child)?;
            }
        }
    }
    let name = allocate_string(machine, key.to_owned())?;
    machine.call_value(reviver, holder, &[name, value])
}

fn stringify<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    let replacer = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED);
    let space = args.get(2).copied().unwrap_or(Value::UNDEFINED);
    let gap = match space.decode() {
        Some(Decoded::Int32(n)) => " ".repeat(((n as i32).max(0) as usize).min(10)),
        Some(Decoded::Number(n)) => " ".repeat((n.max(0.0) as usize).min(10)),
        Some(Decoded::HeapRef(_)) => machine
            .to_string(machine.unbox_primitive_or_self(space)?)?
            .chars()
            .take(10)
            .collect(),
        _ => String::new(),
    };
    let property_list =
        if let Some(r) = replacer.and_then(|r| machine.array_elements(r).ok().flatten()) {
            let mut seen = BTreeSet::new();
            Some(
                r.into_iter()
                    .filter_map(|v| match v.decode() {
                        Some(Decoded::Int32(_) | Decoded::Number(_)) => machine.to_string(v).ok(),
                        Some(Decoded::HeapRef(_)) => machine.string_value(v),
                        _ => None,
                    })
                    .filter(|s| seen.insert(s.clone()))
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
    let callable_replacer = replacer.filter(|value| machine.is_callable(*value).unwrap_or(false));
    let wrapper = machine
        .allocate(HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.object_prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    machine.set_data_property(wrapper, "", value)?;
    let options = SerializeOptions {
        replacer: callable_replacer,
        property_list: property_list.as_deref(),
        gap: &gap,
    };
    let mut stack = Vec::new();
    let Some(text) = serialize_property(machine, wrapper, "", &options, 0, &mut stack)? else {
        return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
    };
    Ok(BuiltinOutcome::Value(allocate_string(machine, text)?))
}
struct SerializeOptions<'a> {
    replacer: Option<Value>,
    property_list: Option<&'a [String]>,
    gap: &'a str,
}
fn serialize_property<H: Host>(
    machine: &mut Machine<'_, H>,
    holder: Value,
    key: &str,
    options: &SerializeOptions<'_>,
    depth: usize,
    stack: &mut Vec<Value>,
) -> Result<Option<String>, EvalFailure> {
    let mut value = machine.get_named_property(holder, key)?;
    if machine.is_object(value) {
        let to_json = machine.get_named_property(value, "toJSON")?;
        if machine.is_callable(to_json)? {
            let k = allocate_string(machine, key.to_owned())?;
            value = machine.call_value(to_json, value, &[k])?
        }
    }
    if let Some(replacer) = options.replacer {
        let k = allocate_string(machine, key.to_owned())?;
        value = machine.call_value(replacer, holder, &[k, value])?
    }
    value = machine.unbox_primitive_or_self(value)?;
    match value.decode() {
        Some(Decoded::Null) => Ok(Some("null".to_owned())),
        Some(Decoded::Boolean(v)) => Ok(Some(v.to_string())),
        Some(Decoded::Int32(v)) => Ok(Some((v as i32).to_string())),
        Some(Decoded::Number(v)) => Ok(Some(if v.is_finite() {
            crate::format_number(v)
        } else {
            "null".to_owned()
        })),
        Some(Decoded::HeapRef(_)) => {
            if let Some(text) = machine.string_value(value) {
                return Ok(Some(quote(&text)));
            }
            if stack.contains(&value) {
                return Err(type_error("Converting circular structure to JSON"));
            }
            if !machine.is_object(value) {
                return Ok(None);
            }
            stack.push(value);
            let result = if let Some(elements) = machine.array_elements(value)? {
                serialize_array(machine, value, elements, options, depth, stack)?
            } else {
                serialize_object(machine, value, options, depth, stack)?
            };
            stack.pop();
            Ok(Some(result))
        }
        Some(Decoded::Undefined | Decoded::Hole | Decoded::Uninitialized) | None => Ok(None),
    }
}
fn serialize_array<H: Host>(
    machine: &mut Machine<'_, H>,
    array: Value,
    elements: Vec<Value>,
    options: &SerializeOptions<'_>,
    depth: usize,
    stack: &mut Vec<Value>,
) -> Result<String, EvalFailure> {
    let mut partial = Vec::with_capacity(elements.len());
    for i in 0..elements.len() {
        partial.push(
            serialize_property(machine, array, &i.to_string(), options, depth + 1, stack)?
                .unwrap_or_else(|| "null".to_owned()),
        )
    }
    Ok(compose('[', ']', partial, options.gap, depth))
}
fn serialize_object<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    options: &SerializeOptions<'_>,
    depth: usize,
    stack: &mut Vec<Value>,
) -> Result<String, EvalFailure> {
    let keys = options
        .property_list
        .map_or_else(|| machine.enumerable_keys(object), |keys| Ok(keys.to_vec()))?;
    let mut partial = Vec::new();
    for key in keys {
        if let Some(value) = serialize_property(machine, object, &key, options, depth + 1, stack)? {
            let sep = if options.gap.is_empty() { ":" } else { ": " };
            partial.push(format!("{}{sep}{value}", quote(&key)))
        }
    }
    Ok(compose('{', '}', partial, options.gap, depth))
}
fn compose(open: char, close: char, parts: Vec<String>, gap: &str, depth: usize) -> String {
    if parts.is_empty() {
        return format!("{open}{close}");
    }
    if gap.is_empty() {
        return format!("{open}{}{close}", parts.join(","));
    }
    let indent = gap.repeat(depth + 1);
    let closing = gap.repeat(depth);
    format!(
        "{open}\n{indent}{}\n{closing}{close}",
        parts.join(&format!(",\n{indent}"))
    )
}
fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
}
impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }
    fn parse<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.ws();
        let value = self.value(machine)?;
        self.ws();
        if self.pos != self.source.len() {
            return Err(self.error_unexpected());
        }
        Ok(value)
    }
    fn ws(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.pos += 1
        }
    }
    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.pos).copied()
    }
    fn value<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::NULL)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::TRUE)
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::FALSE)
            }
            Some(b'"') => self
                .string()
                .and_then(|s| allocate_string(machine, s).map_err(|_| "Out of memory".to_owned())),
            Some(b'[') => self.array(machine),
            Some(b'{') => self.object(machine),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error_unexpected()),
        }
    }
    fn literal(&mut self, literal: &str) -> Result<(), String> {
        if self.source[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.error_unexpected())
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(self.at("Unterminated string in JSON"));
            };
            self.pos += 1;
            match ch {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(self.at("Unterminated string in JSON"));
                    };
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.pos + 4 > self.source.len() {
                                return Err(self.error_unexpected());
                            }
                            let hex = &self.source[self.pos..self.pos + 4];
                            let unit = u16::from_str_radix(hex, 16)
                                .map_err(|_| self.error_unexpected())?;
                            self.pos += 4;
                            out.push_str(&String::from_utf16_lossy(&[unit]))
                        }
                        _ => return Err(self.error_unexpected()),
                    }
                }
                0..=31 => return Err(self.error_unexpected()),
                _ => {
                    let rest = &self.source[self.pos - 1..];
                    let c = rest.chars().next().expect("byte starts char");
                    out.push(c);
                    self.pos += c.len_utf8() - 1
                }
            }
        }
    }
    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1
        }
        if self.peek() == Some(b'0') {
            self.pos += 1;
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(self.at("Unexpected number in JSON"));
            }
        } else {
            self.digits()?
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.digits()?
        }
        if self.peek().is_some_and(|b| matches!(b, b'e' | b'E')) {
            self.pos += 1;
            if self.peek().is_some_and(|b| matches!(b, b'+' | b'-')) {
                self.pos += 1
            }
            self.digits()?
        }
        let n = self.source[start..self.pos]
            .parse::<f64>()
            .map_err(|_| self.error_unexpected())?;
        Ok(crate::number_value(n))
    }
    fn digits(&mut self) -> Result<(), String> {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1
        }
        if self.pos == start {
            Err(self.error_unexpected())
        } else {
            Ok(())
        }
    }
    fn array<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.pos += 1;
        self.ws();
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return allocate_array(machine, out).map_err(|_| "Out of memory".to_owned());
        }
        loop {
            out.push(self.value(machine)?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.ws()
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error_unexpected()),
            }
        }
        allocate_array(machine, out).map_err(|_| "Out of memory".to_owned())
    }
    fn object<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.pos += 1;
        self.ws();
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(|_| "Out of memory".to_owned())?;
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(object);
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.at("Expected property name or '}' in JSON"));
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.at("Expected ':' after property name in JSON"));
            }
            self.pos += 1;
            let value = self.value(machine)?;
            machine
                .set_data_property(object, &key, value)
                .map_err(|_| "Out of memory".to_owned())?;
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.ws()
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error_unexpected()),
            }
        }
        Ok(object)
    }
    fn error_unexpected(&self) -> String {
        match self.peek() {
            None => self.at("Unexpected end of JSON input"),
            Some(b) => {
                let rest = &self.source[self.pos..];
                let token = rest.chars().next().unwrap_or(char::from(b));
                format!(
                    "Unexpected token '{token}', {} is not valid JSON",
                    quote_excerpt(self.source)
                )
            }
        }
    }
    fn at(&self, message: &str) -> String {
        let prefix = &self.source[..self.pos.min(self.source.len())];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = prefix
            .rsplit('\n')
            .next()
            .map_or(1, |s| s.chars().count() + 1);
        format!(
            "{message} at position {} (line {line} column {column})",
            self.pos
        )
    }
}
fn quote_excerpt(source: &str) -> String {
    if source.len() <= 20 {
        format!("\"{source}\"")
    } else {
        format!("\"{}\"...", &source[..20])
    }
}
