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
    let finally_fulfill = install_function(
        heap,
        builtins,
        "Promise finally fulfill",
        1,
        finally_fulfill::<H>,
    );
    let finally_reject = install_function(
        heap,
        builtins,
        "Promise finally reject",
        1,
        finally_reject::<H>,
    );
    builtins.set_promise_finally_targets(finally_fulfill, finally_reject);
    let all_fulfill = install_function(heap, builtins, "Promise all fulfill", 1, all_fulfill::<H>);
    let all_reject = install_function(heap, builtins, "Promise all reject", 1, all_reject::<H>);
    builtins.set_promise_all_targets(all_fulfill, all_reject);

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

fn require_promise_constructor<H: Host>(
    machine: &Machine<'_, H>,
    receiver: Value,
    operation: &'static str,
) -> Result<(), EvalFailure> {
    if machine.intrinsics.global("Promise") == Some(receiver) {
        return Ok(());
    }
    Err(EvalFailure::Throw(ThrowOrigin::TypeError { operation }))
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
    let on_fulfilled = args.first().copied().unwrap_or(Value::UNDEFINED);
    let on_rejected = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    Ok(BuiltinOutcome::Value(machine.promise_then(
        this,
        on_fulfilled,
        on_rejected,
    )?))
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
    require_promise_constructor(machine, this, "Promise.resolve")?;
    Ok(BuiltinOutcome::Value(machine.promise_resolve(
        args.first().copied().unwrap_or(Value::UNDEFINED),
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
    require_promise_constructor(machine, this, "Promise.reject")?;
    Ok(BuiltinOutcome::Value(machine.promise_reject(
        args.first().copied().unwrap_or(Value::UNDEFINED),
    )?))
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
    require_promise_constructor(machine, this, "Promise.all")?;
    Ok(BuiltinOutcome::Value(machine.promise_all(
        args.first().copied().unwrap_or(Value::UNDEFINED),
    )?))
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
    Ok(BuiltinOutcome::Value(machine.promise_then(
        this,
        Value::UNDEFINED,
        args.first().copied().unwrap_or(Value::UNDEFINED),
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
    Ok(BuiltinOutcome::Value(machine.promise_finally(
        this,
        args.first().copied().unwrap_or(Value::UNDEFINED),
    )?))
}

fn finally_fulfill<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally target",
        }));
    }
    let record = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally target",
        }))?;
    machine.fulfill_promise_finally(record)?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
}

fn finally_reject<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally target",
        }));
    }
    let record = args
        .first()
        .copied()
        .ok_or(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "Promise finally target",
        }))?;
    machine.reject_promise_finally(record, args.get(1).copied().unwrap_or(Value::UNDEFINED))?;
    Ok(BuiltinOutcome::Value(Value::UNDEFINED))
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
