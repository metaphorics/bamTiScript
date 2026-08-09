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
    let species = symbol(heap, "Symbol.species");
    let dispose = symbol(heap, "Symbol.dispose");
    let async_dispose = symbol(heap, "Symbol.asyncDispose");
    let unscopables = symbol(heap, "Symbol.unscopables");
    builtins.set_symbol_iterator(iterator);
    builtins.set_symbol_async_iterator(async_iterator);
    builtins.set_symbol_to_string_tag(to_string_tag);
    builtins.set_symbol_species(species);
    builtins.set_symbol_dispose(dispose);
    builtins.set_symbol_async_dispose(async_dispose);
    builtins.set_symbol_unscopables(unscopables);
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
        ("species", species),
        ("dispose", dispose),
        ("asyncDispose", async_dispose),
        ("unscopables", unscopables),
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
        PropertyKey::Named(EcmaString::encode("description")),
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

    globals.insert(EcmaString::encode("Symbol"), constructor);
}

fn symbol(heap: &mut Vec<HeapEntry>, description: &str) -> Value {
    super::super::push(
        heap,
        HeapEntry::Symbol {
            description: EcmaString::encode(description),
        },
    )
}

fn allocate_literal_string(heap: &mut Vec<HeapEntry>, text: &str) -> Value {
    super::super::push(heap, HeapEntry::String(EcmaString::encode(text)))
}

fn define_native_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode(name)),
        builtin_property(value),
    );
}

fn define_readonly_property(heap: &mut [HeapEntry], object: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[heap_index(object)] else {
        unreachable!()
    };
    properties.insert(
        PropertyKey::Named(EcmaString::encode(name)),
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
    let symbol_bytes = key
        .len_units()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or(EvalFailure::Runtime(
            crate::RuntimeErrorKind::HeapByteLimitExceeded {
                limit: machine.limits.max_heap_bytes,
            },
        ))?;
    let registry_bytes = PropertyKey::Named(key.clone()).charge_bytes();
    let total_bytes = symbol_bytes
        .checked_add(registry_bytes)
        .ok_or(EvalFailure::Runtime(
            crate::RuntimeErrorKind::HeapByteLimitExceeded {
                limit: machine.limits.max_heap_bytes,
            },
        ))?;
    machine
        .ensure_allocation_capacity(1, total_bytes)
        .map_err(EvalFailure::Runtime)?;
    let symbol = machine
        .allocate(HeapEntry::Symbol {
            description: key.clone(),
        })
        .map_err(EvalFailure::Runtime)?;
    machine
        .charge_machine(registry_bytes)
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
    use super::super::test_support::{TestHost, blank_program, ordinary_object};
    use super::*;
    use crate::Limits;
    use crate::PropertyMap;
    use crate::ThrowOrigin;
    use crate::intrinsics::{BuiltinDef, BuiltinOutcome, native_function};
    use bamts_bytecode::{FunctionId, ModuleId};

    #[test]
    fn symbol_dispose_is_installed_on_constructor() {
        let module = blank_program("<test>");
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
        let module = blank_program("<test>");
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let key = PropertyKey::Named(EcmaString::encode("dispose"));
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
        let module = blank_program("<test>");
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

    #[test]
    fn symbol_async_dispose_is_installed_readonly_and_stable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let first = machine
            .get_named_property(symbol, "asyncDispose")
            .expect("Symbol.asyncDispose is installed");
        let second = machine
            .get_named_property(symbol, "asyncDispose")
            .expect("Symbol.asyncDispose is readable on second read");
        let description = symbol_description(&machine, first).expect("asyncDispose is a symbol");
        assert!(
            description.eq_ascii("Symbol.asyncDispose"),
            "Symbol.asyncDispose description must be 'Symbol.asyncDispose'"
        );
        assert_eq!(first, second, "Symbol.asyncDispose identity must be stable");
        let key = PropertyKey::Named(EcmaString::encode("asyncDispose"));
        match machine
            .own_descriptor(symbol, &key)
            .expect("descriptor lookup succeeds")
            .expect("Symbol.asyncDispose is defined")
        {
            Property::Data {
                writable,
                enumerable,
                configurable,
                ..
            } => {
                assert!(!writable, "Symbol.asyncDispose must be non-writable");
                assert!(!enumerable, "Symbol.asyncDispose must be non-enumerable");
                assert!(
                    !configurable,
                    "Symbol.asyncDispose must be non-configurable"
                );
            }
            Property::Accessor { .. } => panic!("Symbol.asyncDispose must be a data property"),
        }
    }

    #[test]
    fn symbol_species_descriptor_and_identity_are_stable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let first = machine
            .get_named_property(symbol, "species")
            .expect("Symbol.species is readable");
        let second = machine
            .get_named_property(symbol, "species")
            .expect("Symbol.species is readable twice");
        assert_eq!(first, second);
        assert_eq!(first, machine.intrinsics.builtins.symbol_species());
        let descriptor = machine
            .own_descriptor(symbol, &PropertyKey::Named(EcmaString::encode("species")))
            .expect("descriptor lookup succeeds")
            .expect("Symbol.species is defined");
        assert!(matches!(
            descriptor,
            Property::Data {
                writable: false,
                enumerable: false,
                configurable: false,
                ..
            }
        ));
    }

    #[test]
    fn symbol_unscopables_is_installed_readonly_and_stable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let first = machine
            .get_named_property(symbol, "unscopables")
            .expect("Symbol.unscopables is installed");
        let second = machine
            .get_named_property(symbol, "unscopables")
            .expect("Symbol.unscopables is readable on second read");
        let description = symbol_description(&machine, first).expect("unscopables is a symbol");
        assert!(
            description.eq_ascii("Symbol.unscopables"),
            "Symbol.unscopables description must be 'Symbol.unscopables'"
        );
        assert_eq!(first, second, "Symbol.unscopables identity must be stable");
        assert_eq!(first, machine.intrinsics.builtins.symbol_unscopables());
        let key = PropertyKey::Named(EcmaString::encode("unscopables"));
        match machine
            .own_descriptor(symbol, &key)
            .expect("descriptor lookup succeeds")
            .expect("Symbol.unscopables is defined")
        {
            Property::Data {
                writable,
                enumerable,
                configurable,
                ..
            } => {
                assert!(!writable, "Symbol.unscopables must be non-writable");
                assert!(!enumerable, "Symbol.unscopables must be non-enumerable");
                assert!(!configurable, "Symbol.unscopables must be non-configurable");
            }
            Property::Accessor { .. } => panic!("Symbol.unscopables must be a data property"),
        }
    }

    #[test]
    fn failed_symbol_for_preflight_leaves_heap_and_accounting_unchanged() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let symbol_constructor = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let symbol_for = machine
            .get_named_property(symbol_constructor, "for")
            .expect("Symbol.for is installed");
        let key = machine
            .allocate(HeapEntry::String(EcmaString::encode("key")))
            .expect("registry key allocation succeeds");
        let before = (
            machine.heap.len(),
            machine.heap_bytes,
            machine.machine_bytes,
            machine.intrinsics.symbol_registry.len(),
        );
        machine.limits.max_heap_bytes = machine.heap_bytes + 2 * 3;

        assert!(matches!(
            machine.call_value(symbol_for, symbol_constructor, &[key]),
            Err(EvalFailure::Runtime(
                crate::RuntimeErrorKind::HeapByteLimitExceeded { .. }
            ))
        ));
        assert_eq!(
            (
                machine.heap.len(),
                machine.heap_bytes,
                machine.machine_bytes,
                machine.intrinsics.symbol_registry.len(),
            ),
            before,
            "a failed Symbol.for call must not allocate or charge before registry publication"
        );
    }

    // --- Symbol.hasInstance / instanceof -------------------------------------

    /// A registered builtin that returns the boolean stored on `this` under
    /// `_hasInstanceResult`, modeling a user-defined `@@hasInstance` handler
    /// whose outcome the test controls.
    fn custom_has_instance<H: Host>(
        machine: &mut Machine<'_, H>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let result = machine.get_named_property(this, "_hasInstanceResult")?;
        Ok(BuiltinOutcome::Value(result))
    }

    fn has_instance_property_key(machine: &mut Machine<'_, TestHost>) -> PropertyKey {
        let symbol_constructor = machine
            .intrinsics
            .global("Symbol")
            .expect("Symbol is installed");
        let has_instance = machine
            .get_named_property(symbol_constructor, "hasInstance")
            .expect("Symbol.hasInstance is installed");
        machine
            .to_property_key(has_instance)
            .expect("Symbol.hasInstance is a valid property key")
    }

    /// An ordinary object whose prototype is `prototype`, giving the test
    /// exact control over the value's prototype chain.
    fn object_with_prototype(machine: &mut Machine<'_, TestHost>, prototype: Value) -> Value {
        machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .expect("object allocation succeeds")
    }

    /// A minimal constructor function whose `.prototype` is `prototype`. The
    /// function body is the blank program's halt entry; `instanceof` only
    /// reads `.prototype`, it never calls the constructor.
    fn constructor_with_prototype(machine: &mut Machine<'_, TestHost>, prototype: Value) -> Value {
        let mut properties = PropertyMap::default();
        properties.insert(
            PropertyKey::Named(EcmaString::encode("prototype")),
            Property::Data {
                value: prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
        machine
            .allocate(HeapEntry::Function {
                module: ModuleId::new(0),
                function: FunctionId::new(0),
                captures: Vec::new(),
                context: None,
                properties,
                prototype: Some(machine.intrinsics.function_prototype),
                extensible: true,
            })
            .expect("constructor allocation succeeds")
    }

    #[test]
    fn instanceof_consults_callable_symbol_has_instance() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // The value's prototype chain is unrelated to the constructor, so a
        // true result can only come from the @@hasInstance handler.
        let ctor_proto = ordinary_object(&mut machine);
        let constructor = constructor_with_prototype(&mut machine, ctor_proto);
        let unrelated_proto = ordinary_object(&mut machine);
        let value = object_with_prototype(&mut machine, unrelated_proto);

        let handler_id = machine.intrinsics.builtins.register(BuiltinDef {
            name: "custom hasInstance",
            length: 1,
            handler: custom_has_instance::<TestHost>,
        });
        let handler = native_function(&mut machine.heap, handler_id, "custom hasInstance", 1);
        machine
            .set_data_property(constructor, "_hasInstanceResult", Value::TRUE)
            .unwrap();
        let key = has_instance_property_key(&mut machine);
        machine
            .set_data_property_key(constructor, key, handler)
            .unwrap();

        assert!(
            machine.instance_of(value, constructor).unwrap(),
            "a callable @@hasInstance returning true must make instanceof true"
        );

        // Flip the handler to false: the same value must now be false, proving
        // the handler's result is respected rather than the prototype chain.
        machine
            .set_data_property(constructor, "_hasInstanceResult", Value::FALSE)
            .unwrap();
        assert!(
            !machine.instance_of(value, constructor).unwrap(),
            "a callable @@hasInstance returning false must make instanceof false"
        );
    }

    #[test]
    fn instanceof_falls_back_to_prototype_chain_without_symbol_has_instance() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let ctor_proto = ordinary_object(&mut machine);
        let constructor = constructor_with_prototype(&mut machine, ctor_proto);

        // No @@hasInstance installed: the ordinary prototype-chain check runs.
        let on_chain = object_with_prototype(&mut machine, ctor_proto);
        let off_chain_proto = ordinary_object(&mut machine);
        let off_chain = object_with_prototype(&mut machine, off_chain_proto);

        assert!(
            machine.instance_of(on_chain, constructor).unwrap(),
            "a value on the prototype chain must be an instance without @@hasInstance"
        );
        assert!(
            !machine.instance_of(off_chain, constructor).unwrap(),
            "a value off the prototype chain must not be an instance without @@hasInstance"
        );
    }

    #[test]
    fn instanceof_callable_has_instance_overrides_prototype_chain() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // The value IS on the constructor's prototype chain, so the ordinary
        // check would say true; a @@hasInstance returning false must override.
        let ctor_proto = ordinary_object(&mut machine);
        let constructor = constructor_with_prototype(&mut machine, ctor_proto);
        let on_chain = object_with_prototype(&mut machine, ctor_proto);

        let handler_id = machine.intrinsics.builtins.register(BuiltinDef {
            name: "custom hasInstance override",
            length: 1,
            handler: custom_has_instance::<TestHost>,
        });
        let handler = native_function(
            &mut machine.heap,
            handler_id,
            "custom hasInstance override",
            1,
        );
        machine
            .set_data_property(constructor, "_hasInstanceResult", Value::FALSE)
            .unwrap();
        let key = has_instance_property_key(&mut machine);
        machine
            .set_data_property_key(constructor, key, handler)
            .unwrap();

        assert!(
            !machine.instance_of(on_chain, constructor).unwrap(),
            "a callable @@hasInstance returning false must override a matching prototype chain"
        );
    }

    #[test]
    fn instanceof_non_callable_symbol_has_instance_throws_type_error() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let ctor_proto = ordinary_object(&mut machine);
        let constructor = constructor_with_prototype(&mut machine, ctor_proto);

        // A present but non-callable @@hasInstance is a TypeError per GetMethod.
        let key = has_instance_property_key(&mut machine);
        machine
            .set_data_property_key(constructor, key, Value::int32(42))
            .unwrap();

        let value = ordinary_object(&mut machine);
        assert!(
            matches!(
                machine.instance_of(value, constructor),
                Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
            ),
            "a non-callable @@hasInstance must throw a TypeError"
        );
    }
}
