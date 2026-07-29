use std::collections::{BTreeMap, BTreeSet};

use bamts_bytecode::{EcmaString, EcmaStringBuilder};
use bamts_native::{Decoded, Value};

use super::{allocate_array, allocate_string, define_data, install_function, type_error};
use crate::intrinsics::{BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
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
    globals.insert(EcmaString::from_utf8("JSON"), json);
}

fn parse<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let source = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let value = Parser::new(source.as_units())
        .parse(machine)
        .map_err(|message| {
            let id = machine
                .intrinsics
                .builtins
                .id_named("SyntaxError")
                .expect("SyntaxError installed");
            machine.throw_error(id, message)
        })?;
    if let Some(reviver) = args
        .get(1)
        .copied()
        .filter(|value| machine.is_callable(*value).unwrap_or(false))
    {
        let root = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        let key = EcmaString::default();
        machine.set_data_property_key(root, PropertyKey::Named(key.clone()), value)?;
        return Ok(BuiltinOutcome::Value(walk_reviver(
            machine, root, key, reviver,
        )?));
    }
    Ok(BuiltinOutcome::Value(value))
}

fn walk_reviver<H: Host>(
    machine: &mut Machine<'_, H>,
    holder: Value,
    key: EcmaString,
    reviver: Value,
) -> Result<Value, EvalFailure> {
    let property_key = PropertyKey::Named(key.clone());
    let value = machine.get_property_key(holder, &property_key)?;
    if let Some(elements) = machine.array_elements(value)? {
        for index in 0..elements.len() {
            let name = EcmaString::from_utf8(&index.to_string());
            let child = walk_reviver(machine, value, name.clone(), reviver)?;
            let key = PropertyKey::Named(name);
            if child == Value::UNDEFINED {
                machine.delete_property(value, &key)?;
            } else {
                machine.set_data_property_key(value, key, child)?;
            }
        }
    } else if machine.is_object(value) {
        for name in machine.enumerable_keys(value)? {
            let child = walk_reviver(machine, value, name.clone(), reviver)?;
            let key = PropertyKey::Named(name);
            if child == Value::UNDEFINED {
                machine.delete_property(value, &key)?;
            } else {
                machine.set_data_property_key(value, key, child)?;
            }
        }
    }
    let name = allocate_string(machine, key)?;
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
        Some(Decoded::Int32(number)) => {
            EcmaString::from_utf8(&" ".repeat(((number as i32).max(0) as usize).min(10)))
        }
        Some(Decoded::Number(number)) => {
            EcmaString::from_utf8(&" ".repeat((number.max(0.0) as usize).min(10)))
        }
        Some(Decoded::HeapRef(_)) => {
            let text = machine.to_string(machine.unbox_primitive_or_self(space)?)?;
            text.slice_units(0..text.len_units().min(10))
        }
        _ => EcmaString::default(),
    };
    let property_list = if let Some(values) =
        replacer.and_then(|value| machine.array_elements(value).ok().flatten())
    {
        let mut seen = BTreeSet::new();
        Some(
            values
                .into_iter()
                .filter_map(|value| match value.decode() {
                    Some(Decoded::Int32(_) | Decoded::Number(_)) => machine.to_string(value).ok(),
                    Some(Decoded::HeapRef(_)) => machine.string_value(value),
                    _ => None,
                })
                .filter(|text| seen.insert(text.clone()))
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
    let root_key = EcmaString::default();
    machine.set_data_property_key(wrapper, PropertyKey::Named(root_key.clone()), value)?;
    let options = SerializeOptions {
        replacer: callable_replacer,
        property_list: property_list.as_deref(),
        gap: &gap,
    };
    let mut stack = Vec::new();
    let result = serialize_property(machine, wrapper, root_key, &options, 0, &mut stack)?;
    match result {
        Some(text) => Ok(BuiltinOutcome::Value(allocate_string(machine, text)?)),
        None => Ok(BuiltinOutcome::Value(Value::UNDEFINED)),
    }
}

struct SerializeOptions<'a> {
    replacer: Option<Value>,
    property_list: Option<&'a [EcmaString]>,
    gap: &'a EcmaString,
}

fn serialize_property<H: Host>(
    machine: &mut Machine<'_, H>,
    holder: Value,
    key: EcmaString,
    options: &SerializeOptions<'_>,
    depth: usize,
    stack: &mut Vec<Value>,
) -> Result<Option<EcmaString>, EvalFailure> {
    let mut value = machine.get_property_key(holder, &PropertyKey::Named(key.clone()))?;
    if machine.is_object(value) {
        let to_json = machine.get_named_property(value, "toJSON")?;
        if machine.is_callable(to_json)? {
            let key_value = allocate_string(machine, key.clone())?;
            value = machine.call_value(to_json, value, &[key_value])?;
        }
    }
    if let Some(replacer) = options.replacer {
        let key_value = allocate_string(machine, key)?;
        value = machine.call_value(replacer, holder, &[key_value, value])?;
    }
    value = machine.unbox_primitive_or_self(value)?;
    match value.decode() {
        Some(Decoded::Null) => Ok(Some(EcmaString::from_utf8("null"))),
        Some(Decoded::Boolean(value)) => Ok(Some(EcmaString::from_utf8(if value {
            "true"
        } else {
            "false"
        }))),
        Some(Decoded::Int32(value)) => Ok(Some(EcmaString::from_utf8(&(value as i32).to_string()))),
        Some(Decoded::Number(value)) => {
            let text = if value.is_finite() {
                crate::format_number(value)
            } else {
                "null".to_owned()
            };
            Ok(Some(EcmaString::from_utf8(&text)))
        }
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
                serialize_array(machine, value, elements, options, depth, stack)
            } else {
                serialize_object(machine, value, options, depth, stack)
            };
            stack.pop();
            result.map(Some)
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
) -> Result<EcmaString, EvalFailure> {
    let mut partial = Vec::with_capacity(elements.len());
    for index in 0..elements.len() {
        partial.push(
            serialize_property(
                machine,
                array,
                EcmaString::from_utf8(&index.to_string()),
                options,
                depth + 1,
                stack,
            )?
            .unwrap_or_else(|| EcmaString::from_utf8("null")),
        );
    }
    Ok(compose(b'[', b']', partial, options.gap, depth))
}

fn serialize_object<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
    options: &SerializeOptions<'_>,
    depth: usize,
    stack: &mut Vec<Value>,
) -> Result<EcmaString, EvalFailure> {
    let keys = options
        .property_list
        .map_or_else(|| machine.enumerable_keys(object), |keys| Ok(keys.to_vec()))?;
    let mut partial = Vec::new();
    for key in keys {
        if let Some(value) =
            serialize_property(machine, object, key.clone(), options, depth + 1, stack)?
        {
            let mut member = EcmaStringBuilder::new();
            append(&mut member, &quote(&key));
            member.push_unit(u16::from(b':'));
            if !options.gap.is_empty() {
                member.push_unit(u16::from(b' '));
            }
            append(&mut member, &value);
            partial.push(member.finish());
        }
    }
    Ok(compose(b'{', b'}', partial, options.gap, depth))
}

fn append(output: &mut EcmaStringBuilder, text: &EcmaString) {
    for &unit in text.as_units() {
        output.push_unit(unit);
    }
}

fn compose(
    open: u8,
    close: u8,
    parts: Vec<EcmaString>,
    gap: &EcmaString,
    depth: usize,
) -> EcmaString {
    let mut output = EcmaStringBuilder::new();
    output.push_unit(u16::from(open));
    if parts.is_empty() {
        output.push_unit(u16::from(close));
        return output.finish();
    }
    if gap.is_empty() {
        for (index, part) in parts.iter().enumerate() {
            if index != 0 {
                output.push_unit(u16::from(b','));
            }
            append(&mut output, part);
        }
    } else {
        output.push_unit(u16::from(b'\n'));
        for (index, part) in parts.iter().enumerate() {
            if index != 0 {
                output.push_unit(u16::from(b','));
                output.push_unit(u16::from(b'\n'));
            }
            for _ in 0..=depth {
                append(&mut output, gap);
            }
            append(&mut output, part);
        }
        output.push_unit(u16::from(b'\n'));
        for _ in 0..depth {
            append(&mut output, gap);
        }
    }
    output.push_unit(u16::from(close));
    output.finish()
}

fn push_escape(output: &mut EcmaStringBuilder, escape: &str) {
    output.push_utf8(escape);
}

fn push_hex_escape(output: &mut EcmaStringBuilder, unit: u16) {
    output.push_utf8(&format!("\\u{unit:04x}"));
}

fn quote(text: &EcmaString) -> EcmaString {
    let mut output = EcmaStringBuilder::new();
    output.push_unit(u16::from(b'"'));
    let units = text.as_units();
    let mut offset = 0;
    while offset < units.len() {
        let unit = units[offset];
        match unit {
            0x0022 => push_escape(&mut output, "\\\""),
            0x005C => push_escape(&mut output, "\\\\"),
            0x0008 => push_escape(&mut output, "\\b"),
            0x000C => push_escape(&mut output, "\\f"),
            0x000A => push_escape(&mut output, "\\n"),
            0x000D => push_escape(&mut output, "\\r"),
            0x0009 => push_escape(&mut output, "\\t"),
            0x0000..=0x001F => push_hex_escape(&mut output, unit),
            0xD800..=0xDBFF
                if units
                    .get(offset + 1)
                    .is_some_and(|next| (0xDC00..=0xDFFF).contains(next)) =>
            {
                output.push_unit(unit);
                offset += 1;
                output.push_unit(units[offset]);
            }
            0xD800..=0xDFFF => push_hex_escape(&mut output, unit),
            _ => output.push_unit(unit),
        }
        offset += 1;
    }
    output.push_unit(u16::from(b'"'));
    output.finish()
}

struct Parser<'a> {
    source: &'a [u16],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a [u16]) -> Self {
        Self { source, pos: 0 }
    }

    fn parse<H: Host>(mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
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
            .is_some_and(|unit| matches!(unit, 0x20 | 0x0A | 0x0D | 0x09))
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u16> {
        self.source.get(self.pos).copied()
    }

    fn value<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            Some(unit) if unit == u16::from(b'n') => {
                self.literal("null")?;
                Ok(Value::NULL)
            }
            Some(unit) if unit == u16::from(b't') => {
                self.literal("true")?;
                Ok(Value::TRUE)
            }
            Some(unit) if unit == u16::from(b'f') => {
                self.literal("false")?;
                Ok(Value::FALSE)
            }
            Some(unit) if unit == u16::from(b'"') => {
                let string = self.string()?;
                allocate_string(machine, string).map_err(|_| "Out of memory".to_owned())
            }
            Some(unit) if unit == u16::from(b'[') => self.array(machine),
            Some(unit) if unit == u16::from(b'{') => self.object(machine),
            Some(unit)
                if unit == u16::from(b'-')
                    || (u16::from(b'0')..=u16::from(b'9')).contains(&unit) =>
            {
                self.number()
            }
            _ => Err(self.error_unexpected()),
        }
    }

    fn literal(&mut self, literal: &str) -> Result<(), String> {
        let matches = self.source[self.pos..]
            .iter()
            .zip(literal.bytes())
            .take(literal.len())
            .all(|(unit, byte)| *unit == u16::from(byte));
        if matches && self.source.len() >= self.pos + literal.len() {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.error_unexpected())
        }
    }

    fn string(&mut self) -> Result<EcmaString, String> {
        self.pos += 1;
        let mut output = EcmaStringBuilder::new();
        loop {
            let Some(unit) = self.peek() else {
                return Err(self.at("Unterminated string in JSON"));
            };
            self.pos += 1;
            if unit == u16::from(b'"') {
                return Ok(output.finish());
            }
            if unit == u16::from(b'\\') {
                let Some(escape) = self.peek() else {
                    return Err(self.at("Unterminated string in JSON"));
                };
                self.pos += 1;
                match escape {
                    unit if unit == u16::from(b'"') => output.push_unit(u16::from(b'"')),
                    unit if unit == u16::from(b'\\') => output.push_unit(u16::from(b'\\')),
                    unit if unit == u16::from(b'/') => output.push_unit(u16::from(b'/')),
                    unit if unit == u16::from(b'b') => output.push_unit(0x0008),
                    unit if unit == u16::from(b'f') => output.push_unit(0x000C),
                    unit if unit == u16::from(b'n') => output.push_unit(0x000A),
                    unit if unit == u16::from(b'r') => output.push_unit(0x000D),
                    unit if unit == u16::from(b't') => output.push_unit(0x0009),
                    unit if unit == u16::from(b'u') => output.push_unit(self.hex_unit()?),
                    _ => return Err(self.error_unexpected()),
                }
            } else if unit <= 0x001F {
                return Err(self.error_unexpected());
            } else {
                output.push_unit(unit);
            }
        }
    }

    fn hex_unit(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.source.len() {
            return Err(self.error_unexpected());
        }
        let mut value = 0_u16;
        for &unit in &self.source[self.pos..self.pos + 4] {
            let digit = match unit {
                unit if (u16::from(b'0')..=u16::from(b'9')).contains(&unit) => {
                    unit - u16::from(b'0')
                }
                unit if (u16::from(b'a')..=u16::from(b'f')).contains(&unit) => {
                    unit - u16::from(b'a') + 10
                }
                unit if (u16::from(b'A')..=u16::from(b'F')).contains(&unit) => {
                    unit - u16::from(b'A') + 10
                }
                _ => return Err(self.error_unexpected()),
            };
            value = value * 16 + digit;
        }
        self.pos += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(u16::from(b'-')) {
            self.pos += 1;
        }
        if self.peek() == Some(u16::from(b'0')) {
            self.pos += 1;
            if self
                .peek()
                .is_some_and(|unit| (u16::from(b'0')..=u16::from(b'9')).contains(&unit))
            {
                return Err(self.at("Unexpected number in JSON"));
            }
        } else {
            self.digits()?;
        }
        if self.peek() == Some(u16::from(b'.')) {
            self.pos += 1;
            self.digits()?;
        }
        if self
            .peek()
            .is_some_and(|unit| unit == u16::from(b'e') || unit == u16::from(b'E'))
        {
            self.pos += 1;
            if self
                .peek()
                .is_some_and(|unit| unit == u16::from(b'+') || unit == u16::from(b'-'))
            {
                self.pos += 1;
            }
            self.digits()?;
        }
        let bytes: Vec<u8> = self.source[start..self.pos]
            .iter()
            .map(|unit| *unit as u8)
            .collect();
        let number = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .ok_or_else(|| self.error_unexpected())?;
        Ok(crate::number_value(number))
    }

    fn digits(&mut self) -> Result<(), String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|unit| (u16::from(b'0')..=u16::from(b'9')).contains(&unit))
        {
            self.pos += 1;
        }
        (self.pos != start)
            .then_some(())
            .ok_or_else(|| self.error_unexpected())
    }

    fn array<H: Host>(&mut self, machine: &mut Machine<'_, H>) -> Result<Value, String> {
        self.pos += 1;
        self.ws();
        let mut output = Vec::new();
        if self.peek() == Some(u16::from(b']')) {
            self.pos += 1;
            return allocate_array(machine, output).map_err(|_| "Out of memory".to_owned());
        }
        loop {
            output.push(self.value(machine)?);
            self.ws();
            match self.peek() {
                Some(unit) if unit == u16::from(b',') => {
                    self.pos += 1;
                    self.ws();
                }
                Some(unit) if unit == u16::from(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error_unexpected()),
            }
        }
        allocate_array(machine, output).map_err(|_| "Out of memory".to_owned())
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
        if self.peek() == Some(u16::from(b'}')) {
            self.pos += 1;
            return Ok(object);
        }
        loop {
            if self.peek() != Some(u16::from(b'"')) {
                return Err(self.at("Expected property name or '}' in JSON"));
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(u16::from(b':')) {
                return Err(self.at("Expected ':' after property name in JSON"));
            }
            self.pos += 1;
            let value = self.value(machine)?;
            machine
                .set_data_property_key(object, PropertyKey::Named(key), value)
                .map_err(|_| "Out of memory".to_owned())?;
            self.ws();
            match self.peek() {
                Some(unit) if unit == u16::from(b',') => {
                    self.pos += 1;
                    self.ws();
                }
                Some(unit) if unit == u16::from(b'}') => {
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
            Some(unit) if unit <= 0x7F => self.at(&format!(
                "Unexpected token '{}' in JSON",
                char::from_u32(u32::from(unit)).expect("ASCII unit is scalar")
            )),
            Some(unit) => self.at(&format!("Unexpected code unit U+{unit:04X} in JSON")),
        }
    }

    fn at(&self, message: &str) -> String {
        let prefix = &self.source[..self.pos.min(self.source.len())];
        let line = prefix
            .iter()
            .filter(|unit| **unit == u16::from(b'\n'))
            .count()
            + 1;
        let column = prefix
            .iter()
            .rposition(|unit| *unit == u16::from(b'\n'))
            .map_or(prefix.len() + 1, |offset| prefix.len() - offset);
        format!(
            "{message} at position {} (line {line} column {column})",
            self.pos
        )
    }
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Verified,
    };

    use super::*;
    use crate::Limits;

    #[derive(Default)]
    struct TestHost;

    impl Host for TestHost {}

    fn module() -> Program<Verified> {
        let code = Module::new(
            vec![Constant::String(EcmaString::from_utf8("<test>"))],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![Instruction::Halt],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("valid test module");
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("valid test program")
    }

    fn call_json(machine: &mut Machine<'_, TestHost>, method: &str, source: EcmaString) -> Value {
        let json = machine.intrinsics.global("JSON").expect("JSON exists");
        let function = machine
            .get_named_property(json, method)
            .expect("method exists");
        let source = allocate_string(machine, source).expect("string allocation succeeds");
        machine
            .call_value(function, json, &[source])
            .expect("JSON call succeeds")
    }

    #[test]
    fn parse_ignores_non_callable_revivers() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let json = machine.intrinsics.global("JSON").expect("JSON exists");
        let parse = machine
            .get_named_property(json, "parse")
            .expect("JSON.parse exists");
        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds");

        for reviver in [Value::NULL, Value::FALSE, object] {
            let source = allocate_string(&mut machine, EcmaString::from_utf8("1"))
                .expect("string allocation succeeds");
            let value = machine
                .call_value(parse, json, &[source, reviver])
                .expect("non-callable reviver is ignored");
            assert_eq!(value, Value::int32(1));
        }
    }

    #[test]
    fn parse_preserves_utf16_surrogate_units() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let lone = call_json(&mut machine, "parse", EcmaString::from_utf8("\"\\uD83D\""));
        assert_eq!(machine.string_value(lone).unwrap().as_units(), &[0xD83D]);
        let pair = call_json(
            &mut machine,
            "parse",
            EcmaString::from_utf8("\"\\uD83D\\uDE03\""),
        );
        assert_eq!(
            machine.string_value(pair).unwrap().as_units(),
            &[0xD83D, 0xDE03]
        );
    }

    #[test]
    fn stringify_escapes_unpaired_surrogates() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let json = machine.intrinsics.global("JSON").expect("JSON exists");
        let stringify = machine.get_named_property(json, "stringify").unwrap();
        let value = allocate_string(&mut machine, EcmaString::from_units(&[0xD800])).unwrap();
        let result = machine.call_value(stringify, json, &[value]).unwrap();
        assert!(
            machine
                .string_value(result)
                .unwrap()
                .eq_ascii("\"\\ud800\"")
        );
    }
}
