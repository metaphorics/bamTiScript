use std::collections::BTreeMap;
use std::ops::Range;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use super::{allocate_array, allocate_string, define_data, install_function, type_error};
use crate::intrinsics::regexp::{Match, Regex};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let constructor = install_function(heap, builtins, "RegExp", 2, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    for (name, length, handler) in [
        ("exec", 1, exec::<H> as BuiltinHandler<H>),
        ("test", 1, test::<H>),
        ("toString", 0, to_string::<H>),
    ] {
        let function = install_function(heap, builtins, name, length, handler);
        define_data(heap, prototype, name, function);
        globals.insert(EcmaString::from_utf8(&format!("\0RegExp.{name}")), function);
    }
    globals.insert(EcmaString::from_utf8("\0RegExp.prototype"), prototype);
    globals.insert(EcmaString::from_utf8("RegExp"), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (pattern, inherited_flags) = args.first().copied().map_or_else(
        || Ok((EcmaString::default(), EcmaString::default())),
        |value| {
            if let Some(parts) = regexp_parts(machine, value) {
                Ok(parts)
            } else {
                Ok((machine.to_string(value)?, EcmaString::default()))
            }
        },
    )?;
    let flags = if let Some(value) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    {
        machine.to_string(value)?
    } else {
        inherited_flags
    };
    compile(machine, &pattern, &flags)?;
    let mut properties = PropertyMap::default();
    for (name, value, writable) in [
        (
            "source",
            allocate_string(
                machine,
                if pattern.is_empty() {
                    EcmaString::from_utf8("(?:)")
                } else {
                    pattern.clone()
                },
            )?,
            false,
        ),
        (
            "flags",
            allocate_string(
                machine,
                Regex::compile(&pattern, &flags)
                    .expect("validated")
                    .flags()
                    .canonical(),
            )?,
            false,
        ),
        ("lastIndex", Value::int32(0), true),
    ] {
        properties.insert(
            PropertyKey::Named(EcmaString::from_utf8(name)),
            Property::Data {
                value,
                writable,
                enumerable: false,
                configurable: false,
            },
        );
    }
    let prototype = machine.intrinsics.regexp_prototype();
    let value = machine
        .allocate(HeapEntry::RegExp {
            pattern,
            flags,
            properties,
            prototype: Some(prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(value))
}

pub(super) fn compile<H: Host>(
    machine: &mut Machine<'_, H>,
    pattern: &EcmaString,
    flags: &EcmaString,
) -> Result<Regex, EvalFailure> {
    Regex::compile(pattern, flags).map_err(|error| {
        let id = machine
            .intrinsics
            .builtins
            .id_named("SyntaxError")
            .expect("SyntaxError installed");
        machine.throw_error(id, error.message().to_owned())
    })
}

pub(super) fn regexp_parts<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Option<(EcmaString, EcmaString)> {
    let index = machine.runtime_slot(value).ok().flatten()?;
    match &machine.heap[index] {
        HeapEntry::RegExp { pattern, flags, .. } => Some((pattern.clone(), flags.clone())),
        _ => None,
    }
}

pub(super) fn execute<H: Host>(
    machine: &mut Machine<'_, H>,
    regexp: Value,
    input: &EcmaString,
) -> Result<Option<Match>, EvalFailure> {
    let (pattern, flags) = regexp_parts(machine, regexp)
        .ok_or_else(|| type_error("RegExp method called on incompatible receiver"))?;
    let regex = compile(machine, &pattern, &flags)?;
    let uses_last_index = regex.flags().global || regex.flags().sticky;
    let start = if uses_last_index {
        index_value(machine.get_named_property(regexp, "lastIndex")?)
    } else {
        0
    };
    let matched = regex.exec(input, start);
    if uses_last_index {
        let next = matched.as_ref().map_or(0, |value| value.range.end);
        machine.set_data_property(regexp, "lastIndex", crate::number_value(next as f64))?;
    }
    Ok(matched)
}

fn exec<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let Some(matched) = execute(machine, this, &input)? else {
        return Ok(BuiltinOutcome::Value(Value::NULL));
    };
    Ok(BuiltinOutcome::Value(match_array(
        machine, &input, matched,
    )?))
}

fn test<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let input = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::boolean(
        execute(machine, this, &input)?.is_some(),
    )))
}

fn to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (source, flags) = regexp_parts(machine, this)
        .ok_or_else(|| type_error("RegExp method called on incompatible receiver"))?;
    let mut output = bamts_bytecode::EcmaStringBuilder::new();
    output.push_unit(u16::from(b'/'));
    for &unit in source.as_units() {
        if unit == u16::from(b'/') {
            output.push_unit(u16::from(b'\\'));
        }
        output.push_unit(unit);
    }
    output.push_unit(u16::from(b'/'));
    for &unit in flags.as_units() {
        output.push_unit(unit);
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}

pub(super) fn match_array<H: Host>(
    machine: &mut Machine<'_, H>,
    input: &EcmaString,
    matched: Match,
) -> Result<Value, EvalFailure> {
    let mut values = Vec::with_capacity(matched.captures.len());
    for capture in &matched.captures {
        values.push(match capture {
            Some(range) => allocate_string(machine, slice_units(input, range.clone()))?,
            None => Value::UNDEFINED,
        });
    }
    let array = allocate_array(machine, values)?;
    machine.set_data_property(
        array,
        "index",
        crate::number_value(matched.range.start as f64),
    )?;
    let input_value = allocate_string(machine, input.clone())?;
    machine.set_data_property(array, "input", input_value)?;
    if matched.named.is_empty() {
        machine.set_data_property(array, "groups", Value::UNDEFINED)?;
    } else {
        let groups = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: None,
                extensible: true,
                boxed_primitive: None,
            })
            .map_err(EvalFailure::Runtime)?;
        for (name, range) in matched.named {
            let value = match range {
                Some(range) => allocate_string(machine, slice_units(input, range))?,
                None => Value::UNDEFINED,
            };
            machine.set_data_property(groups, &name, value)?;
        }
        machine.set_data_property(array, "groups", groups)?;
    }
    Ok(array)
}

pub(super) fn slice_units(input: &EcmaString, range: Range<usize>) -> EcmaString {
    input.slice_units(range)
}
fn index_value(value: Value) -> usize {
    match value.decode() {
        Some(bamts_native::Decoded::Int32(value)) => value as usize,
        Some(bamts_native::Decoded::Number(value)) if value.is_finite() && value > 0.0 => {
            value as usize
        }
        _ => 0,
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

    fn construct_regexp(machine: &mut Machine<'_, TestHost>, pattern: &str, flags: &str) -> Value {
        let constructor = machine.intrinsics.global("RegExp").expect("RegExp exists");
        let pattern = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8(pattern)))
            .unwrap();
        let flags = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8(flags)))
            .unwrap();
        let index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction { id, .. } = machine.heap[index] else {
            panic!("RegExp constructor is native")
        };
        let BuiltinOutcome::Value(value) = machine
            .call_builtin(id, Value::UNDEFINED, &[pattern, flags], true)
            .unwrap()
        else {
            panic!("RegExp constructor returns a value")
        };
        value
    }

    #[test]
    fn sticky_last_index_is_a_utf16_code_unit_offset() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let regexp = construct_regexp(&mut machine, "x", "y");
        machine
            .set_data_property(regexp, "lastIndex", Value::int32(2))
            .unwrap();

        let matched = execute(&mut machine, regexp, &EcmaString::from_utf8("😀x"))
            .unwrap()
            .unwrap();

        assert_eq!(matched.range, 2..3);
        assert_eq!(
            machine.get_named_property(regexp, "lastIndex").unwrap(),
            Value::int32(3)
        );
    }

    #[test]
    fn exec_index_after_astral_prefix_is_a_code_unit_offset() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let regexp = construct_regexp(&mut machine, "x", "");
        let input = EcmaString::from_utf8("😀x");
        let matched = execute(&mut machine, regexp, &input).unwrap().unwrap();

        let result = match_array(&mut machine, &input, matched).unwrap();

        assert_eq!(
            machine.get_named_property(result, "index").unwrap(),
            Value::int32(2)
        );
    }
}
