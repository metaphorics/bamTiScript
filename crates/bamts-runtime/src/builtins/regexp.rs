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
    }
    builtins.set_regexp_prototype(prototype);
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
            allocate_string(machine, canonical_source(&pattern))?,
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
    let (pattern, flags) = regexp_parts(machine, this)
        .ok_or_else(|| type_error("RegExp method called on incompatible receiver"))?;
    let mut output = bamts_bytecode::EcmaStringBuilder::new();
    output.push_unit(u16::from(b'/'));
    append_canonical_source(&pattern, &mut output);
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

pub(crate) fn canonical_source(pattern: &EcmaString) -> EcmaString {
    let mut escaped = bamts_bytecode::EcmaStringBuilder::new();
    append_canonical_source(pattern, &mut escaped);
    escaped.finish()
}

fn append_canonical_source(pattern: &EcmaString, output: &mut bamts_bytecode::EcmaStringBuilder) {
    if pattern.is_empty() {
        output.push_utf8("(?:)");
        return;
    }

    let mut previous_was_escape = false;
    let mut in_character_class = false;
    for &unit in pattern.as_units() {
        match unit {
            0x005B if !previous_was_escape => {
                in_character_class = true;
                output.push_unit(unit);
            }
            0x005D if !previous_was_escape => {
                in_character_class = false;
                output.push_unit(unit);
            }
            0x002F if !previous_was_escape && !in_character_class => output.push_utf8("\\/"),
            0x000A => output.push_utf8("\\n"),
            0x000D => output.push_utf8("\\r"),
            0x2028 => output.push_utf8("\\u2028"),
            0x2029 => output.push_utf8("\\u2029"),
            _ => output.push_unit(unit),
        }
        previous_was_escape = unit == 0x005C && !previous_was_escape;
    }
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        BinaryOp, Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module,
        ModuleId, Program, ProgramModule, Register, Verified,
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
        let HeapEntry::NativeFunction {
            callable: crate::NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
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

    #[test]
    fn source_escapes_solidus_and_line_terminator() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        for (pattern, expected) in [("/", "\\/"), ("\n", "\\n")] {
            let regexp = construct_regexp(&mut machine, pattern, "");
            let source = machine.get_named_property(regexp, "source").unwrap();
            assert_eq!(
                machine.to_string(source).unwrap(),
                EcmaString::from_utf8(expected)
            );
        }
    }

    /// Builds a program that materializes a RegExp via `Instruction::CreateRegExp`
    /// (the literal bytecode path, which stores the raw pattern with no own
    /// `source` property), reads `.source`, and returns `source === expected`.
    fn literal_source_program(pattern: &str, flags: &str, expected: &str) -> Program<Verified> {
        let mut constants = vec![
            Constant::String(EcmaString::from_utf8(pattern)),
            Constant::String(EcmaString::from_utf8(flags)),
            Constant::String(EcmaString::from_utf8("source")),
            Constant::String(EcmaString::from_utf8(expected)),
        ];
        let name = ConstantId::new(constants.len() as u32);
        constants.push(Constant::String(EcmaString::from_utf8("<test>")));
        let code = Module::new(
            constants,
            vec![Function::new(
                None,
                0,
                0,
                5,
                FunctionFlags::default(),
                vec![
                    Instruction::CreateRegExp {
                        dst: Register::new(0),
                        pattern: ConstantId::new(0),
                        flags: ConstantId::new(1),
                    },
                    Instruction::LoadConst {
                        dst: Register::new(1),
                        constant: ConstantId::new(2),
                    },
                    Instruction::GetProperty {
                        dst: Register::new(2),
                        object: Register::new(0),
                        key: Register::new(1),
                    },
                    Instruction::LoadConst {
                        dst: Register::new(3),
                        constant: ConstantId::new(3),
                    },
                    Instruction::Binary {
                        dst: Register::new(4),
                        op: BinaryOp::StrictEqual,
                        left: Register::new(2),
                        right: Register::new(3),
                    },
                    Instruction::Return {
                        value: Register::new(4),
                    },
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("valid test module");
        Program::link(
            vec![ProgramModule {
                name,
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("valid test program")
    }

    #[test]
    fn source_and_to_string_preserve_rou3_character_class_solidus() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        for (pattern, source, string) in [
            (
                r"^/users/(?<id>[^/]+)/?$",
                r"^\/users\/(?<id>[^/]+)\/?$",
                r"/^\/users\/(?<id>[^/]+)\/?$/",
            ),
            (
                r"^\/users\/(?<id>[^\/]+)\/?$",
                r"^\/users\/(?<id>[^\/]+)\/?$",
                r"/^\/users\/(?<id>[^\/]+)\/?$/",
            ),
            (r"\/", r"\/", r"/\//"),
        ] {
            let regexp = construct_regexp(&mut machine, pattern, "");
            let source_value = machine.get_named_property(regexp, "source").unwrap();
            assert_eq!(machine.to_string(source_value).unwrap(), EcmaString::from_utf8(source));

            let BuiltinOutcome::Value(string_value) =
                to_string(&mut machine, regexp, &[], false).unwrap()
            else {
                panic!("RegExp toString returns a value");
            };
            assert_eq!(
                machine.to_string(string_value).unwrap(),
                EcmaString::from_utf8(string)
            );
        }
    }

    #[test]
    fn literal_source_is_canonicalized() {
        // The literal bytecode path (`Instruction::CreateRegExp`) stores the raw
        // pattern and owns no `source` data property, so `.source` must be
        // canonicalized at read time rather than at allocation. A solidus and a
        // line terminator both exercise the shared canonicalizer; the program
        // returns `re.source === expected`, proving the literal path no longer
        // leaks the raw pattern (which would differ from the constructor).
        for (pattern, flags, expected) in [
            ("/", "", "\\/"),
            ("\n", "", "\\n"),
            (
                r"^/users/(?<id>[^/]+)/?$",
                "",
                r"^\/users\/(?<id>[^/]+)\/?$",
            ),
        ] {
            let program = literal_source_program(pattern, flags, expected);
            let mut host = TestHost;
            let execution = Machine::new(&program, &mut host, Limits::default())
                .run()
                .unwrap();
            assert_eq!(execution.value, Value::TRUE);
        }

        // The ASCII fast path (`get_named_property` -> `own_get_ascii`) hits the
        // other source fallback on a `CreateRegExp`-shaped RegExp (empty own
        // properties), confirming both read paths share one canonicalizer.
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let regexp = machine
            .allocate(HeapEntry::RegExp {
                pattern: EcmaString::from_utf8("/"),
                flags: EcmaString::default(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.regexp_prototype()),
                extensible: true,
            })
            .unwrap();
        let source = machine.get_named_property(regexp, "source").unwrap();
        assert_eq!(
            machine.to_string(source).unwrap(),
            EcmaString::from_utf8("\\/")
        );
    }
}
