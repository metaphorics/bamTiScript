use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use super::{
    allocate_string, builtin_property, define_data, heap_index, install_function, type_error,
};
use crate::intrinsics::{BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let iterator = symbol(heap, "Symbol.iterator");
    let async_iterator = symbol(heap, "Symbol.asyncIterator");
    let has_instance = symbol(heap, "Symbol.hasInstance");
    let to_string_tag = symbol(heap, "Symbol.toStringTag");
    let dispose = symbol(heap, "Symbol.dispose");
    builtins.set_symbol_iterator(iterator);
    builtins.set_symbol_async_iterator(async_iterator);
    builtins.set_symbol_to_string_tag(to_string_tag);
    builtins.set_symbol_prototype(prototype);

    let constructor = install_function(heap, builtins, "Symbol", 0, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    let symbol_for = install_function(heap, builtins, "for", 1, symbol_for::<H>);
    define_native_property(heap, constructor, "for", symbol_for);
    for (name, value) in [
        ("iterator", iterator),
        ("asyncIterator", async_iterator),
        ("hasInstance", has_instance),
        ("toStringTag", to_string_tag),
        ("dispose", dispose),
    ] {
        define_readonly_property(heap, constructor, name, value);
    }

    let to_string = install_function(heap, builtins, "toString", 0, to_string::<H>);
    let value_of = install_function(heap, builtins, "valueOf", 0, value_of::<H>);
    let description = install_function(heap, builtins, "get description", 0, description::<H>);
    define_data(heap, prototype, "toString", to_string);
    define_data(heap, prototype, "valueOf", value_of);
    let symbol_tag = allocate_literal_string(heap, "Symbol");
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8("description")),
        Property::Accessor {
            getter: Some(description),
            setter: None,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        PropertyKey::Symbol(heap_index(to_string_tag) as u32),
        builtin_property(symbol_tag),
    );

    globals.insert(EcmaString::from_utf8("Symbol"), constructor);
}

fn symbol(heap: &mut Vec<HeapEntry>, description: &str) -> Value {
    super::super::push(
        heap,
        HeapEntry::Symbol {
            description: EcmaString::from_utf8(description),
        },
    )
}

fn allocate_literal_string(heap: &mut Vec<HeapEntry>, text: &str) -> Value {
    super::super::push(heap, HeapEntry::String(EcmaString::from_utf8(text)))
}

fn define_native_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        builtin_property(value),
    );
}

fn define_readonly_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        Property::Data {
            value,
            writable: false,
            enumerable: false,
            configurable: false,
        },
    );
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error("Symbol is not a constructor"));
    }
    let description = args
        .first()
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
        .map(|value| machine.to_string(value))
        .transpose()?
        .unwrap_or_default();
    let symbol = machine
        .allocate(HeapEntry::Symbol { description })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(symbol))
}

fn symbol_for<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let key = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    if let Some(existing) = machine.intrinsics.symbol_registry.get(&key).copied() {
        return Ok(BuiltinOutcome::Value(existing));
    }
    let symbol = machine
        .allocate(HeapEntry::Symbol {
            description: key.clone(),
        })
        .map_err(EvalFailure::Runtime)?;
    machine
        .charge_machine(PropertyKey::Named(key.clone()).charge_bytes())
        .map_err(EvalFailure::Runtime)?;
    machine.intrinsics.symbol_registry.insert(key, symbol);
    Ok(BuiltinOutcome::Value(symbol))
}

fn symbol_description<H: Host>(
    machine: &Machine<'_, H>,
    value: Value,
) -> Result<EcmaString, EvalFailure> {
    let Some(index) = machine.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
        return Err(type_error("Symbol method called on incompatible receiver"));
    };
    match &machine.heap[index] {
        HeapEntry::Symbol { description } => Ok(description.clone()),
        _ => Err(type_error("Symbol method called on incompatible receiver")),
    }
}

fn description<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let description = symbol_description(machine, this)?;
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        description,
    )?))
}

fn to_string<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let description = symbol_description(machine, this)?;
    let mut builder =
        bamts_bytecode::EcmaStringBuilder::with_capacity(description.len_units().saturating_add(8));
    builder.push_utf8("Symbol(");
    for &unit in description.as_units() {
        builder.push_unit(unit);
    }
    builder.push_unit(u16::from(b')'));
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        builder.finish(),
    )?))
}

fn value_of<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    symbol_description(machine, this)?;
    Ok(BuiltinOutcome::Value(this))
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

    #[test]
    fn symbol_dispose_is_installed_on_constructor() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let dispose = machine
            .get_named_property(symbol, "dispose")
            .expect("Symbol.dispose is installed");
        let description = symbol_description(&machine, dispose).expect("dispose is a symbol");
        assert!(
            description.eq_ascii("Symbol.dispose"),
            "Symbol.dispose description must be 'Symbol.dispose'"
        );
    }

    #[test]
    fn symbol_dispose_descriptor_is_readonly_non_enumerable_non_configurable() {
        let module = module();
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let key = PropertyKey::Named(EcmaString::from_utf8("dispose"));
        let descriptor = machine
            .own_descriptor(symbol, &key)
            .expect("descriptor lookup succeeds")
            .expect("Symbol.dispose is defined");
        match descriptor {
            Property::Data {
                writable,
                enumerable,
                configurable,
                ..
            } => {
                assert!(!writable, "Symbol.dispose must be non-writable");
                assert!(!enumerable, "Symbol.dispose must be non-enumerable");
                assert!(!configurable, "Symbol.dispose must be non-configurable");
            }
            Property::Accessor { .. } => panic!("Symbol.dispose must be a data property"),
        }
    }

    #[test]
    fn symbol_dispose_identity_is_stable_across_reads() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let first = machine
            .get_named_property(symbol, "dispose")
            .expect("Symbol.dispose is readable");
        let second = machine
            .get_named_property(symbol, "dispose")
            .expect("Symbol.dispose is readable on second read");
        assert_eq!(
            first, second,
            "Symbol.dispose identity must be stable across reads"
        );
    }
}
