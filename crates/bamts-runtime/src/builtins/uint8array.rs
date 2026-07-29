use std::collections::BTreeMap;

use bamts_bytecode::{EcmaString, EcmaStringBuilder};
use bamts_native::{Decoded, Value};

use super::{
    allocate_string, builtin_property, define_data, heap_index, install_function, type_error,
};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, Property, PropertyKey, PropertyMap};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = super::super::ordinary_prototype(heap, builtins.object_prototype());
    let constructor = install_function(heap, builtins, "Uint8Array", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    define_data(heap, prototype, "constructor", constructor);
    let join = install_function(heap, builtins, "join", 1, join::<H> as BuiltinHandler<H>);
    define_data(heap, prototype, "join", join);
    let tag = super::super::push(heap, HeapEntry::String(EcmaString::from_utf8("Uint8Array")));
    let HeapEntry::Object { properties, .. } = &mut heap[heap_index(prototype)] else {
        unreachable!("Uint8Array prototype is ordinary")
    };
    properties.insert(
        PropertyKey::Symbol(heap_index(builtins.symbol_to_string_tag()) as u32),
        builtin_property(tag),
    );
    globals.insert(EcmaString::from_utf8("Uint8Array"), constructor);
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(type_error("Uint8Array constructor requires 'new'"));
    }
    let values = match args.first().copied() {
        None | Some(Value::UNDEFINED) => Vec::new(),
        Some(source) => machine.iterable_values(source)?,
    };
    let length = values.len();
    let mut properties = PropertyMap::default();
    for (index, value) in values.into_iter().enumerate() {
        properties.insert(
            PropertyKey::Named(EcmaString::from_utf8(&index.to_string())),
            Property::Data {
                value: Value::int32(u32::from(to_uint8(machine, value)?)),
                writable: true,
                enumerable: true,
                configurable: true,
            },
        );
    }
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8("length")),
        Property::Data {
            value: crate::number_value(length as f64),
            writable: false,
            enumerable: false,
            configurable: false,
        },
    );
    let prototype = constructor_prototype(machine)?;
    let value = machine
        .allocate(HeapEntry::Object {
            properties,
            prototype: Some(prototype),
            extensible: true,
            boxed_primitive: None,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(value))
}

fn to_uint8<H: Host>(machine: &mut Machine<'_, H>, value: Value) -> Result<u8, EvalFailure> {
    let number = machine.to_number_observable(value)?;
    let number = match number.decode() {
        Some(Decoded::Int32(value)) => f64::from(value as i32),
        Some(Decoded::Number(value)) => value,
        _ => unreachable!("ToNumber produces a numeric value"),
    };
    Ok(if number.is_finite() && number != 0.0 {
        number.trunc().rem_euclid(256.0) as u8
    } else {
        0
    })
}

fn join<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    _: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let prototype = constructor_prototype(machine)?;
    if !machine.inherits_from_prototype(this, prototype)? {
        return Err(type_error(
            "Uint8Array.prototype.join called on incompatible receiver",
        ));
    }
    let length = machine.get_named_property(this, "length")?;
    let length = match length.decode() {
        Some(Decoded::Int32(value)) => value as usize,
        Some(Decoded::Number(value)) if value.is_finite() && value >= 0.0 => value as usize,
        _ => {
            return Err(type_error(
                "Uint8Array.prototype.join called on incompatible receiver",
            ));
        }
    };
    let separator = match args.first().copied() {
        None | Some(Value::UNDEFINED) => EcmaString::from_utf8(","),
        Some(value) => machine.to_string_observable(value)?,
    };
    let mut output = EcmaStringBuilder::new();
    for index in 0..length {
        if index != 0 {
            for &unit in separator.as_units() {
                output.push_unit(unit);
            }
        }
        let value = machine.get_named_property(this, &index.to_string())?;
        if !matches!(value.decode(), Some(Decoded::Undefined | Decoded::Null)) {
            for &unit in machine.to_string_observable(value)?.as_units() {
                output.push_unit(unit);
            }
        }
    }
    Ok(BuiltinOutcome::Value(allocate_string(
        machine,
        output.finish(),
    )?))
}

fn constructor_prototype<H: Host>(machine: &Machine<'_, H>) -> Result<Value, EvalFailure> {
    let constructor = machine
        .intrinsics
        .global("Uint8Array")
        .ok_or_else(|| type_error("missing Uint8Array constructor"))?;
    let index = machine
        .runtime_slot(constructor)
        .map_err(EvalFailure::Runtime)?
        .ok_or_else(|| type_error("invalid Uint8Array constructor"))?;
    let HeapEntry::NativeFunction { properties, .. } = &machine.heap[index] else {
        return Err(type_error("invalid Uint8Array constructor"));
    };
    match properties.get(&PropertyKey::Named(EcmaString::from_utf8("prototype"))) {
        Some(Property::Data { value, .. }) => Ok(*value),
        _ => Err(type_error("missing Uint8Array prototype")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Verified,
    };

    use super::*;
    use crate::intrinsics::{BuiltinDef, native_function};
    use crate::{Limits, NativeCallable, PropertyMap, ThrowOrigin};

    static NEXT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ITERATION_COMPLETE: AtomicBool = AtomicBool::new(false);

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

    fn object(machine: &mut Machine<'_, TestHost>) -> Value {
        machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds")
    }

    fn iterator_method(
        _: &mut Machine<'_, TestHost>,
        this: Value,
        _: &[Value],
        _: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Ok(BuiltinOutcome::Value(this))
    }

    fn iterator_next(
        machine: &mut Machine<'_, TestHost>,
        this: Value,
        _: &[Value],
        _: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let done = NEXT_CALLS.fetch_add(1, Ordering::SeqCst) != 0;
        if done {
            ITERATION_COMPLETE.store(true, Ordering::SeqCst);
        }
        let result = object(machine);
        machine.set_data_property(result, "done", Value::boolean(done))?;
        if !done {
            machine.set_data_property(result, "value", this)?;
        }
        Ok(BuiltinOutcome::Value(result))
    }

    fn value_of(
        _: &mut Machine<'_, TestHost>,
        _: Value,
        _: &[Value],
        _: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        assert!(
            ITERATION_COMPLETE.load(Ordering::SeqCst),
            "Uint8Array must finish iterable collection before coercing elements"
        );
        Ok(BuiltinOutcome::Value(Value::int32(257)))
    }

    fn construct(machine: &mut Machine<'_, TestHost>, argument: Value) -> Value {
        let constructor = machine
            .intrinsics
            .global("Uint8Array")
            .expect("Uint8Array installs");
        let index = machine
            .runtime_slot(constructor)
            .expect("valid constructor")
            .expect("heap");
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("Uint8Array is native")
        };
        let BuiltinOutcome::Value(value) = machine
            .call_builtin(id, Value::UNDEFINED, &[argument], true)
            .expect("constructor succeeds")
        else {
            panic!("constructor returns an object")
        };
        value
    }

    #[test]
    fn uint8array_collects_before_coercion_and_exposes_bounded_surface() {
        NEXT_CALLS.store(0, Ordering::SeqCst);
        ITERATION_COMPLETE.store(false, Ordering::SeqCst);
        let program = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let source = object(&mut machine);
        let iterator = native(&mut machine, "[Symbol.iterator]", iterator_method);
        let next = native(&mut machine, "next", iterator_next);
        let value_of = native(&mut machine, "valueOf", value_of);
        let iterator_symbol = machine.intrinsics.builtins.symbol_iterator();
        let iterator_key = machine
            .to_property_key(iterator_symbol)
            .expect("symbol key");
        machine
            .set_data_property_key(source, iterator_key, iterator)
            .expect("iterator install succeeds");
        machine
            .set_data_property(source, "next", next)
            .expect("next install succeeds");
        machine
            .set_data_property(source, "valueOf", value_of)
            .expect("valueOf install succeeds");

        let typed = construct(&mut machine, source);
        assert_eq!(
            machine.get_named_property(typed, "length").unwrap(),
            Value::int32(1)
        );
        assert_eq!(
            machine.get_named_property(typed, "0").unwrap(),
            Value::int32(1)
        );
        let join = machine
            .get_named_property(typed, "join")
            .expect("join inherits");
        let joined = machine.call_value(join, typed, &[]).expect("join succeeds");
        assert!(
            machine
                .string_value(joined)
                .is_some_and(|text| text.eq_ascii("1"))
        );

        let element = object(&mut machine);
        machine
            .set_data_property(element, "valueOf", value_of)
            .expect("valueOf install succeeds");
        let array_input = machine
            .allocate(HeapEntry::Array {
                elements: vec![element],
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .expect("array allocation succeeds");
        let from_array = construct(&mut machine, array_input);
        let array_join = machine
            .get_named_property(from_array, "join")
            .expect("join inherits");
        let array_joined = machine
            .call_value(array_join, from_array, &[])
            .expect("join succeeds");
        assert!(
            machine
                .string_value(array_joined)
                .is_some_and(|text| text.eq_ascii("1"))
        );

        let constructor = machine.intrinsics.global("Uint8Array").unwrap();
        let prototype = machine
            .get_named_property(constructor, "prototype")
            .unwrap();
        assert_eq!(
            machine
                .get_named_property(prototype, "constructor")
                .unwrap(),
            constructor
        );
        let array = machine.intrinsics.global("Array").unwrap();
        let is_array = machine.get_named_property(array, "isArray").unwrap();
        assert_eq!(
            machine.call_value(is_array, array, &[typed]).unwrap(),
            Value::FALSE
        );
        let plain = object(&mut machine);
        machine
            .set_data_property(plain, "length", Value::int32(1))
            .unwrap();
        assert!(matches!(
            machine.call_value(join, plain, &[]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
    }
}
