use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use super::{define_data, define_to_string_tag, install_function};
use crate::intrinsics::BuiltinOutcome;
use crate::intrinsics::BuiltinTable;
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyKey, PropertyMap, ThrowOrigin};

pub(super) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = crate::intrinsics::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(builtins.object_prototype()),
            boxed_primitive: None,
            extensible: true,
        },
    );
    builtins.set_promise_prototype(prototype);
    let resolve_target = install_function(
        heap,
        builtins,
        "Promise resolve target",
        1,
        resolve_target::<H>,
    );
    let reject_target = install_function(
        heap,
        builtins,
        "Promise reject target",
        1,
        reject_target::<H>,
    );
    builtins.set_promise_resolver_targets(resolve_target, reject_target);
    let all_fulfill = install_function(heap, builtins, "Promise all fulfill", 1, all_fulfill::<H>);
    let all_reject = install_function(heap, builtins, "Promise all reject", 1, all_reject::<H>);
    builtins.set_promise_all_targets(all_fulfill, all_reject);
    let capability_executor = install_function(
        heap,
        builtins,
        "Promise capability executor",
        2,
        capability_executor::<H>,
    );
    let finally_value = install_function(
        heap,
        builtins,
        "Promise finally value",
        1,
        finally_value::<H>,
    );
    let finally_throw = install_function(
        heap,
        builtins,
        "Promise finally throw",
        1,
        finally_throw::<H>,
    );
    let species = install_function(heap, builtins, "get [Symbol.species]", 0, species::<H>);
    let finally_return = install_function(
        heap,
        builtins,
        "Promise finally return",
        0,
        finally_return::<H>,
    );
    let finally_rethrow = install_function(
        heap,
        builtins,
        "Promise finally rethrow",
        0,
        finally_rethrow::<H>,
    );
    let then_fulfill = install_function(
        heap,
        builtins,
        "Promise capability fulfill",
        1,
        then_fulfill::<H>,
    );
    let then_reject = install_function(
        heap,
        builtins,
        "Promise capability reject",
        1,
        then_reject::<H>,
    );

    let constructor = install_function(heap, builtins, "Promise", 1, constructor::<H>);
    builtins.set_constructor_prototype(heap, constructor, prototype);
    globals.insert(EcmaString::from_utf8("Promise"), constructor);
    let queue_microtask =
        install_function(heap, builtins, "queueMicrotask", 1, queue_microtask::<H>);
    globals.insert(EcmaString::from_utf8("queueMicrotask"), queue_microtask);

    let resolve = install_function(heap, builtins, "resolve", 1, resolve::<H>);
    let reject = install_function(heap, builtins, "reject", 1, reject::<H>);
    let all = install_function(heap, builtins, "all", 1, all::<H>);
    define_static(heap, constructor, "resolve", resolve);
    define_static(heap, constructor, "reject", reject);
    define_static(heap, constructor, "all", all);
    define_static(
        heap,
        constructor,
        "\0capabilityExecutor",
        capability_executor,
    );
    define_static(heap, constructor, "\0finallyValue", finally_value);
    define_static(heap, constructor, "\0finallyThrow", finally_throw);
    define_static(heap, constructor, "\0finallyReturn", finally_return);
    define_static(heap, constructor, "\0finallyRethrow", finally_rethrow);
    define_static(heap, constructor, "\0thenFulfill", then_fulfill);
    define_static(heap, constructor, "\0thenReject", then_reject);
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        unreachable!("Promise constructor is native")
    };
    properties.insert(
        PropertyKey::Symbol(super::heap_index(builtins.symbol_species()) as u32),
        crate::Property::Accessor {
            getter: Some(species),
            setter: None,
            enumerable: false,
            configurable: true,
        },
    );
    let then = install_function(heap, builtins, "then", 2, then::<H>);
    let catch = install_function(heap, builtins, "catch", 1, catch::<H>);
    let finally = install_function(heap, builtins, "finally", 1, finally::<H>);
    define_data(heap, prototype, "constructor", constructor);
    define_data(heap, prototype, "then", then);
    define_data(heap, prototype, "catch", catch);
    define_data(heap, prototype, "finally", finally);
    define_to_string_tag(heap, prototype, builtins.symbol_to_string_tag(), "Promise");
}

fn define_static(heap: &mut [HeapEntry], constructor: Value, name: &str, value: Value) {
    let HeapEntry::NativeFunction { properties, .. } = &mut heap[super::heap_index(constructor)]
    else {
        panic!("Promise constructor must be native")
    };
    properties.insert(
        PropertyKey::Named(EcmaString::from_utf8(name)),
        super::builtin_property(value),
    );
}

struct PromiseCapability {
    promise: Value,
    resolve: Value,
    reject: Value,
}

fn intrinsic_target<H: Host>(
    machine: &mut Machine<'_, H>,
    name: &str,
) -> Result<Value, EvalFailure> {
    let constructor = machine
        .intrinsics
        .global("Promise")
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise constructor",
        }))?;
    machine.get_named_property(constructor, name)
}

fn new_promise_capability<H: Host>(
    machine: &mut Machine<'_, H>,
    constructor: Value,
) -> Result<PromiseCapability, EvalFailure> {
    if !machine.is_callable(constructor)? {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise constructor",
        }));
    }
    let record = super::ordinary_runtime(machine, None)?;
    let target = intrinsic_target(machine, "\0capabilityExecutor")?;
    let executor = machine.create_promise_resolver_function(target, record)?;
    let promise = machine.construct_value(constructor, &[executor])?;
    let resolve = machine.get_named_property(record, "resolve")?;
    let reject = machine.get_named_property(record, "reject")?;
    if !machine.is_callable(resolve)? || !machine.is_callable(reject)? {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise capability executor",
        }));
    }
    Ok(PromiseCapability {
        promise,
        resolve,
        reject,
    })
}

fn promise_resolve_with_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    constructor: Value,
    value: Value,
) -> Result<Value, EvalFailure> {
    if matches!(
        machine.runtime_slot(value).map_err(EvalFailure::Runtime)?,
        Some(index) if matches!(machine.heap[index], HeapEntry::Promise { .. })
    ) {
        let value_constructor = machine.get_named_property(value, "constructor")?;
        if value_constructor == constructor {
            return Ok(value);
        }
    }
    let capability = new_promise_capability(machine, constructor)?;
    machine.call_value(capability.resolve, Value::UNDEFINED, &[value])?;
    Ok(capability.promise)
}

fn reject_capability_failure<H: Host>(
    machine: &mut Machine<'_, H>,
    capability: &PromiseCapability,
    failure: EvalFailure,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (reason, _) = machine.promise_rejection_value(failure)?;
    machine.call_value(capability.reject, Value::UNDEFINED, &[reason])?;
    Ok(BuiltinOutcome::Value(capability.promise))
}

fn capability_executor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise capability executor",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    if machine.get_named_property(record, "resolve")? != Value::UNDEFINED {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise capability executor",
        }));
    }
    machine.set_data_property(
        record,
        "resolve",
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    machine.set_data_property(
        record,
        "reject",
        args.get(2).copied().unwrap_or(Value::UNDEFINED),
    )?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn species<H: Host>(
    _machine: &mut Machine<'_, H>,
    this: Value,
    _args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise Symbol.species",
        }));
    }
    Ok(BuiltinOutcome::Value(this))
}

fn species_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    object: Value,
) -> Result<Value, EvalFailure> {
    let default = machine
        .intrinsics
        .global("Promise")
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise constructor",
        }))?;
    let constructor = machine.get_named_property(object, "constructor")?;
    if matches!(constructor.decode(), Some(bamts_native::Decoded::Undefined)) {
        return Ok(default);
    }
    if !machine.is_object(constructor) {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise constructor",
        }));
    }
    let species_symbol = machine.intrinsics.builtins.symbol_species();
    let species_key = machine.to_property_key(species_symbol)?;
    let species = machine.get_property_key(constructor, &species_key)?;
    if matches!(
        species.decode(),
        Some(bamts_native::Decoded::Undefined | bamts_native::Decoded::Null)
    ) {
        return Ok(default);
    }
    if !machine.is_callable(species)? {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise Symbol.species",
        }));
    }
    Ok(species)
}

fn constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise constructor",
        }));
    }
    let executor = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !machine.is_callable(executor)? {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise executor",
        }));
    }

    let promise = machine.create_promise()?;
    let new_target = machine.current_new_target();
    if new_target != Value::UNDEFINED {
        let default_prototype = machine.intrinsics.builtins.promise_prototype();
        let prototype = machine
            .constructed_prototype(new_target)
            .unwrap_or(default_prototype);
        if prototype != default_prototype {
            machine.set_prototype_value(promise, Some(prototype))?;
        }
    }
    let record = machine.create_promise_resolver(promise)?;
    let (resolve_target, reject_target) = machine.intrinsics.builtins.promise_resolver_targets();
    let resolve = machine.create_promise_resolver_function(resolve_target, record)?;
    let reject = machine.create_promise_resolver_function(reject_target, record)?;
    if let Err(failure) = machine.call_value(executor, Value::UNDEFINED, &[resolve, reject]) {
        machine.reject_promise_resolver_failure(record, failure)?;
    }
    Ok(BuiltinOutcome::Value(promise))
}

fn resolve_target<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise resolve",
        }));
    }
    let record = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise resolver",
        }))?;
    machine.resolve_promise_resolver(record, args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn reject_target<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise reject",
        }));
    }
    let record = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise resolver",
        }))?;
    machine.reject_promise_resolver(record, args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn then<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.prototype.then",
        }));
    }
    if !matches!(
        machine.runtime_slot(this).map_err(EvalFailure::Runtime)?,
        Some(index) if matches!(machine.heap[index], HeapEntry::Promise { .. })
    ) {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.prototype.then",
        }));
    }
    let constructor = species_constructor(machine, this)?;
    let capability = new_promise_capability(machine, constructor)?;
    let record = super::ordinary_runtime(machine, None)?;
    machine.set_data_property(
        record,
        "onFulfilled",
        args.first().copied().unwrap_or(Value::UNDEFINED),
    )?;
    machine.set_data_property(
        record,
        "onRejected",
        args.get(1).copied().unwrap_or(Value::UNDEFINED),
    )?;
    machine.set_data_property(record, "resolve", capability.resolve)?;
    machine.set_data_property(record, "reject", capability.reject)?;
    let fulfill_target = intrinsic_target(machine, "\0thenFulfill")?;
    let reject_target = intrinsic_target(machine, "\0thenReject")?;
    let fulfill = machine.create_promise_resolver_function(fulfill_target, record)?;
    let reject = machine.create_promise_resolver_function(reject_target, record)?;
    let ignored = machine.promise_then(this, fulfill, reject)?;
    let _ = ignored;
    Ok(BuiltinOutcome::Value(capability.promise))
}

fn resolve<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.resolve",
        }));
    }
    let value = args.first().copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(promise_resolve_with_constructor(
        machine, this, value,
    )?))
}

fn reject<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.reject",
        }));
    }
    let capability = new_promise_capability(machine, this)?;
    machine.call_value(
        capability.reject,
        Value::UNDEFINED,
        &[args.first().copied().unwrap_or(Value::UNDEFINED)],
    )?;
    Ok(BuiltinOutcome::Value(capability.promise))
}

fn all<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.all",
        }));
    }
    let capability = new_promise_capability(machine, this)?;
    let resolve = match machine.get_named_property(this, "resolve") {
        Ok(resolve) if machine.is_callable(resolve)? => resolve,
        Ok(_) => {
            return reject_capability_failure(
                machine,
                &capability,
                EvalFailure::Throw(ThrowOrigin::TypeError {
                    operation: "Promise.all resolve",
                }),
            );
        }
        Err(failure) => return reject_capability_failure(machine, &capability, failure),
    };
    let iterable = args.first().copied().unwrap_or(Value::UNDEFINED);
    let aggregate = machine.promise_all_with_resolve(iterable, resolve, this)?;
    let then = machine.get_named_property(aggregate, "then")?;
    if let Err(failure) =
        machine.call_value(then, aggregate, &[capability.resolve, capability.reject])
    {
        return reject_capability_failure(machine, &capability, failure);
    }
    Ok(BuiltinOutcome::Value(capability.promise))
}

fn catch<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.prototype.catch",
        }));
    }
    let then = machine.get_named_property(this, "then")?;
    Ok(BuiltinOutcome::Value(machine.call_value(
        then,
        this,
        &[
            Value::UNDEFINED,
            args.first().copied().unwrap_or(Value::UNDEFINED),
        ],
    )?))
}

fn finally<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.prototype.finally",
        }));
    }
    if !machine.is_object(this) {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise.prototype.finally",
        }));
    }
    let species = species_constructor(machine, this)?;
    let handler = args.first().copied().unwrap_or(Value::UNDEFINED);
    let (on_fulfilled, on_rejected) = if machine.is_callable(handler)? {
        let record = super::ordinary_runtime(machine, None)?;
        machine.set_data_property(record, "handler", handler)?;
        machine.set_data_property(record, "constructor", species)?;
        let fulfill = intrinsic_target(machine, "\0finallyValue")?;
        let reject = intrinsic_target(machine, "\0finallyThrow")?;
        (
            machine.create_promise_resolver_function(fulfill, record)?,
            machine.create_promise_resolver_function(reject, record)?,
        )
    } else {
        (handler, handler)
    };
    let then = machine.get_named_property(this, "then")?;
    Ok(BuiltinOutcome::Value(machine.call_value(
        then,
        this,
        &[on_fulfilled, on_rejected],
    )?))
}

fn reject_reaction_failure<H: Host>(
    machine: &mut Machine<'_, H>,
    record: Value,
    failure: EvalFailure,
) -> Result<(), EvalFailure> {
    let (reason, _) = machine.promise_rejection_value(failure)?;
    let reject = machine.get_named_property(record, "reject")?;
    machine.call_value(reject, Value::UNDEFINED, &[reason])?;
    Ok(())
}

fn then_fulfill<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise reaction",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    let value = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let handler = machine.get_named_property(record, "onFulfilled")?;
    let result = if machine.is_callable(handler)? {
        match machine.call_value(handler, Value::UNDEFINED, &[value]) {
            Ok(result) => result,
            Err(failure) => {
                reject_reaction_failure(machine, record, failure)?;
                return Ok(BuiltinOutcome::Value(Value::UNDEFINED));
            }
        }
    } else {
        value
    };
    let resolve = machine.get_named_property(record, "resolve")?;
    machine.call_value(resolve, Value::UNDEFINED, &[result])?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn then_reject<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise reaction",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    let reason = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    let handler = machine.get_named_property(record, "onRejected")?;
    if machine.is_callable(handler)? {
        match machine.call_value(handler, Value::UNDEFINED, &[reason]) {
            Ok(result) => {
                let resolve = machine.get_named_property(record, "resolve")?;
                machine.call_value(resolve, Value::UNDEFINED, &[result])?;
            }
            Err(failure) => reject_reaction_failure(machine, record, failure)?,
        }
    } else {
        let reject = machine.get_named_property(record, "reject")?;
        machine.call_value(reject, Value::UNDEFINED, &[reason])?;
    }
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn finally_wait<H: Host>(
    machine: &mut Machine<'_, H>,
    record: Value,
    original: Value,
    rejected: bool,
) -> Result<Value, EvalFailure> {
    let handler = machine.get_named_property(record, "handler")?;
    let result = machine.call_value(handler, Value::UNDEFINED, &[])?;
    let constructor = machine.get_named_property(record, "constructor")?;
    let promise = promise_resolve_with_constructor(machine, constructor, result)?;
    let continuation_record = super::ordinary_runtime(machine, None)?;
    machine.set_data_property(continuation_record, "value", original)?;
    let target = intrinsic_target(
        machine,
        if rejected {
            "\0finallyRethrow"
        } else {
            "\0finallyReturn"
        },
    )?;
    let continuation = machine.create_promise_resolver_function(target, continuation_record)?;
    let then = machine.get_named_property(promise, "then")?;
    machine.call_value(then, promise, &[continuation, Value::UNDEFINED])
}

fn finally_value<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally callback",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    let original = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(finally_wait(
        machine, record, original, false,
    )?))
}

fn finally_throw<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally callback",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    let original = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(finally_wait(
        machine, record, original, true,
    )?))
}

fn finally_return<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally continuation",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(
        machine.get_named_property(record, "value")?,
    ))
}

fn finally_rethrow<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally continuation",
        }));
    }
    let record = args.first().copied().unwrap_or(Value::UNDEFINED);
    Err(EvalFailure::ThrowValue(
        machine.get_named_property(record, "value")?,
    ))
}

fn all_fulfill<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise all target",
        }));
    }
    let element = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise all target",
        }))?;
    machine
        .resolve_promise_all_element(element, args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn all_reject<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise all target",
        }));
    }
    let element = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise all target",
        }))?;
    machine
        .reject_promise_all_element(element, args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn queue_microtask<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "queueMicrotask",
        }));
    }
    let callback = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !machine.is_callable(callback)? {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "queueMicrotask callback",
        }));
    }
    machine.enqueue_microtask_callback(callback)?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TestHost, blank_program};
    use super::*;
    use crate::intrinsics::BuiltinHandler;
    use crate::{Limits, PromiseState};

    fn callback(
        machine: &mut Machine<'_, TestHost>,
        name: &'static str,
        handler: BuiltinHandler<TestHost>,
    ) -> Value {
        install_function(
            &mut machine.heap,
            &mut machine.intrinsics.builtins,
            name,
            0,
            handler,
        )
    }

    fn return_undefined(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Ok(BuiltinOutcome::Value(Value::UNDEFINED))
    }

    fn throw_99(
        _machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Err(EvalFailure::ThrowValue(Value::int32(99)))
    }

    fn resolve_88(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine
            .globals
            .insert(EcmaString::from_utf8("thenSeen"), Value::int32(2));
        machine.call_value(
            args.first().copied().unwrap_or(Value::UNDEFINED),
            Value::UNDEFINED,
            &[Value::int32(88)],
        )?;
        Ok(BuiltinOutcome::Value(Value::UNDEFINED))
    }

    fn return_thenable(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine
            .globals
            .insert(EcmaString::from_utf8("handlerSeen"), Value::int32(1));
        Ok(BuiltinOutcome::Value(
            machine.globals[&EcmaString::from_utf8("thenable")],
        ))
    }

    fn iterator_method(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        Ok(BuiltinOutcome::Value(
            machine.globals[&EcmaString::from_utf8("testIterator")],
        ))
    }

    fn iterator_next_once(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let seen = machine
            .globals
            .insert(EcmaString::from_utf8("nextSeen"), Value::TRUE)
            .is_some();
        Ok(BuiltinOutcome::Value(
            machine.iterator_result(Value::int32(1), seen)?,
        ))
    }

    fn iterator_return(
        machine: &mut Machine<'_, TestHost>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine
            .globals
            .insert(EcmaString::from_utf8("iteratorClosed"), Value::TRUE);
        Ok(BuiltinOutcome::Value(this))
    }

    fn throwing_resolve(
        machine: &mut Machine<'_, TestHost>,
        this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        machine
            .globals
            .insert(EcmaString::from_utf8("resolveReceiver"), this);
        Err(EvalFailure::ThrowValue(Value::int32(99)))
    }

    fn resolve_getter(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let count = machine
            .globals
            .get(&EcmaString::from_utf8("resolveGetCount"))
            .and_then(|value| value.decode())
            .and_then(|value| match value {
                bamts_native::Decoded::Int32(raw) => Some(raw),
                _ => None,
            })
            .unwrap_or(0);
        machine.globals.insert(
            EcmaString::from_utf8("resolveGetCount"),
            Value::int32(count + 1),
        );
        Ok(BuiltinOutcome::Value(
            machine.globals[&EcmaString::from_utf8("throwingResolve")],
        ))
    }

    fn one_value_iterable(machine: &mut Machine<'_, TestHost>) -> Value {
        let iterator = super::super::ordinary_runtime(machine, None).unwrap();
        let next = callback(machine, "test iterator next", iterator_next_once);
        let close = callback(machine, "test iterator return", iterator_return);
        machine.set_data_property(iterator, "next", next).unwrap();
        machine
            .set_data_property(iterator, "return", close)
            .unwrap();
        machine
            .globals
            .insert(EcmaString::from_utf8("testIterator"), iterator);

        let iterable = super::super::ordinary_runtime(machine, None).unwrap();
        let method = callback(machine, "test iterator method", iterator_method);
        let symbol = machine.intrinsics.builtins.symbol_iterator();
        let key = machine.to_property_key(symbol).unwrap();
        machine
            .set_data_property_key(iterable, key, method)
            .unwrap();
        iterable
    }

    #[test]
    fn all_calls_receiver_resolve_and_closes_on_abrupt_completion() {
        let module = blank_program("<promise-all-test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let constructor = machine.intrinsics.global("Promise").unwrap();
        let resolve = callback(&mut machine, "throwing resolve", throwing_resolve);
        machine
            .globals
            .insert(EcmaString::from_utf8("throwingResolve"), resolve);
        let getter = callback(&mut machine, "resolve getter", resolve_getter);
        machine
            .define_accessor(
                constructor,
                PropertyKey::Named(EcmaString::from_utf8("resolve")),
                getter,
                crate::AccessorKind::Getter,
            )
            .unwrap();
        let iterable = one_value_iterable(&mut machine);
        let all = machine.get_named_property(constructor, "all").unwrap();

        let promise = machine
            .call_value(all, constructor, &[iterable])
            .expect("Promise.all returns its rejected capability");
        machine.drain_microtasks().unwrap();

        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("resolveGetCount")),
            Some(&Value::int32(1))
        );
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("resolveReceiver")),
            Some(&constructor)
        );
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("iteratorClosed")),
            Some(&Value::TRUE)
        );
        assert!(
            matches!(state(&machine, promise), PromiseState::Rejected { reason, .. } if reason == Value::int32(99))
        );
    }

    fn state(machine: &Machine<'_, TestHost>, promise: Value) -> PromiseState {
        let index = machine.runtime_slot(promise).unwrap().unwrap();
        let HeapEntry::Promise { state, .. } = &machine.heap[index] else {
            panic!("expected promise");
        };
        state.clone()
    }

    #[test]
    fn finally_preserves_fulfillment_and_rejection_and_replaces_with_throw() {
        let module = blank_program("<promise-finally-test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let pass = callback(&mut machine, "pass finally", return_undefined);
        let throw = callback(&mut machine, "throw finally", throw_99);

        let fulfilled = machine.create_promise().unwrap();
        let finally = machine.get_named_property(fulfilled, "finally").unwrap();
        let preserved = machine.call_value(finally, fulfilled, &[pass]).unwrap();
        machine.fulfill_promise(fulfilled, Value::int32(7)).unwrap();
        machine.drain_microtasks().unwrap();
        assert!(
            matches!(state(&machine, preserved), PromiseState::Fulfilled { value } if value == Value::int32(7))
        );

        let rejected = machine.create_promise().unwrap();
        let preserved = machine.call_value(finally, rejected, &[pass]).unwrap();
        machine
            .reject_promise(rejected, Value::int32(8), ThrowOrigin::Bytecode)
            .unwrap();
        machine.drain_microtasks().unwrap();
        assert!(
            matches!(state(&machine, preserved), PromiseState::Rejected { reason, .. } if reason == Value::int32(8))
        );

        let source = machine.create_promise().unwrap();
        let replaced = machine.call_value(finally, source, &[throw]).unwrap();
        machine.fulfill_promise(source, Value::int32(7)).unwrap();
        machine.drain_microtasks().unwrap();
        assert!(
            matches!(state(&machine, replaced), PromiseState::Rejected { reason, .. } if reason == Value::int32(99))
        );
    }

    #[test]
    fn finally_awaits_thenable_before_preserving_completion() {
        let module = blank_program("<promise-finally-test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let then = callback(&mut machine, "thenable then", resolve_88);
        let handler = callback(&mut machine, "return thenable", return_thenable);
        let thenable = super::super::ordinary_runtime(&mut machine, None).unwrap();
        machine.set_data_property(thenable, "then", then).unwrap();
        machine
            .globals
            .insert(EcmaString::from_utf8("thenable"), thenable);

        let source = machine.create_promise().unwrap();
        let finally = machine.get_named_property(source, "finally").unwrap();
        let derived = machine.call_value(finally, source, &[handler]).unwrap();
        machine.fulfill_promise(source, Value::int32(7)).unwrap();
        machine.drain_microtasks().unwrap();

        assert_eq!(
            machine.globals.get(&EcmaString::from_utf8("handlerSeen")),
            Some(&Value::int32(1))
        );
        assert_eq!(
            machine.globals.get(&EcmaString::from_utf8("thenSeen")),
            Some(&Value::int32(2))
        );
        assert!(
            matches!(state(&machine, derived), PromiseState::Fulfilled { value } if value == Value::int32(7))
        );
    }

    #[test]
    fn constructor_honors_new_target_for_subclass_prototype() {
        let module = blank_program("<promise-subclass-test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let promise_constructor = machine.intrinsics.global("Promise").unwrap();
        let promise_prototype = machine.intrinsics.builtins.promise_prototype();

        // Build a subclass prototype that inherits from Promise.prototype.
        let subclass_prototype =
            super::super::ordinary_runtime(&mut machine, Some(promise_prototype)).unwrap();
        machine
            .set_data_property(subclass_prototype, "myMethod", Value::int32(42))
            .unwrap();

        // Build a new_target whose .prototype is the subclass prototype.
        let new_target = super::super::ordinary_runtime(&mut machine, None).unwrap();
        machine
            .set_data_property(new_target, "prototype", subclass_prototype)
            .unwrap();

        let promise_id = machine.intrinsics.builtins.id_named("Promise").unwrap();
        let executor = callback(&mut machine, "resolve immediately", return_undefined);
        let BuiltinOutcome::Value(promise) = machine
            .call_builtin_with_new_target(
                promise_id,
                Value::UNDEFINED,
                &[executor],
                true,
                new_target,
            )
            .unwrap()
        else {
            panic!("Promise construct returns a value");
        };

        // The promise must inherit from the subclass prototype, not
        // Promise.prototype directly.
        assert_eq!(
            machine.prototype_value(promise).unwrap(),
            Some(subclass_prototype)
        );
        // Subclass methods are visible on the instance.
        assert_eq!(
            machine.get_named_property(promise, "myMethod").unwrap(),
            Value::int32(42)
        );
        // And it is still a real promise.
        assert!(matches!(
            state(&machine, promise),
            PromiseState::Pending { .. }
        ));
        let _ = promise_constructor;
    }

    #[test]
    fn then_rejects_non_promise_receiver_without_observable_getter() {
        let module = blank_program("<promise-then-brand-test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        // A spy object with a `constructor` getter that would be observed
        // if the brand check did not short-circuit first.
        let spy = super::super::ordinary_runtime(&mut machine, None).unwrap();
        let getter = callback(&mut machine, "constructor getter", constructor_getter);
        machine
            .define_accessor(
                spy,
                PropertyKey::Named(EcmaString::from_utf8("constructor")),
                getter,
                crate::AccessorKind::Getter,
            )
            .unwrap();

        let then = machine
            .get_named_property(machine.intrinsics.builtins.promise_prototype(), "then")
            .unwrap();
        let result = machine.call_value(then, spy, &[Value::UNDEFINED]);

        // Must throw a TypeError.
        assert!(matches!(
            result,
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        // The constructor getter must NOT have been invoked.
        assert_eq!(
            machine
                .globals
                .get(&EcmaString::from_utf8("constructorGetCount")),
            None
        );
    }

    fn constructor_getter(
        machine: &mut Machine<'_, TestHost>,
        _this: Value,
        _args: &[Value],
        _constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let count = machine
            .globals
            .get(&EcmaString::from_utf8("constructorGetCount"))
            .and_then(|value| value.decode())
            .and_then(|value| match value {
                bamts_native::Decoded::Int32(raw) => Some(raw),
                _ => None,
            })
            .unwrap_or(0);
        machine.globals.insert(
            EcmaString::from_utf8("constructorGetCount"),
            Value::int32(count + 1),
        );
        Ok(BuiltinOutcome::Value(
            machine.intrinsics.global("Promise").unwrap(),
        ))
    }
}
