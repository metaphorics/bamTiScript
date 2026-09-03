//! The `Function` constructor global (ECMA-262 19.2).
//!
//! The object installs unconditionally: test262 harness code and ordinary
//! guest code read `Function.prototype` for `call`/`bind` plumbing without
//! ever synthesizing a function, so an absent `Function` global fails every
//! such script at load time with an unbound-global ReferenceError. Dynamic
//! `Function(...)` synthesis additionally requires the host to provide a
//! script compiler; without one the constructor throws a controlled
//! `TypeError` at the call instead of being absent, keeping the failure at
//! the one place that needs dynamic compilation.

use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use super::{define_data, define_frozen_data, install_constructor_function};
use crate::intrinsics::{BuiltinHandler, BuiltinOutcome, BuiltinTable};
use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyMap, ScriptSource, ThrowOrigin};

pub(crate) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    globals: &mut BTreeMap<EcmaString, Value>,
    builtins: &mut BuiltinTable<H>,
) {
    let prototype = builtins.function_prototype();
    let constructor = install_constructor_function(
        heap,
        builtins,
        "Function",
        1,
        function_constructor::<H> as BuiltinHandler<H>,
    );
    define_frozen_data(heap, constructor, "prototype", prototype);
    define_data(heap, prototype, "constructor", constructor);
    globals.insert(EcmaString::encode("Function"), constructor);
}

/// `Function(p1, ..., pn, body)` / `new Function(p1, ..., pn, body)`.
///
/// The arguments synthesize into a `function anonymous(...) { ... }`
/// expression compiled as a classic script; classic scripts return their
/// completion value, so the entry's result is the new function itself.
fn function_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    _constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let (parameter_source, body_source) = match args.split_last() {
        None => (String::new(), String::new()),
        Some((body, parameters)) => {
            let mut names = Vec::with_capacity(parameters.len());
            for parameter in parameters {
                let text = machine.to_string(*parameter)?;
                names.push(String::from_utf16_lossy(text.as_units()));
            }
            let body_text = machine.to_string(*body)?;
            (
                names.join(","),
                String::from_utf16_lossy(body_text.as_units()),
            )
        }
    };
    // Line offsets inside the body must match the spec's synthesis, so the
    // body starts on its own source line.
    let mut source = String::from("(function anonymous(");
    source.push_str(&parameter_source);
    source.push_str("\n)\n{\n");
    source.push_str(&body_source);
    source.push_str("\n})");
    let script = EcmaString::encode(&source);
    let script_name = EcmaString::encode("Function constructor");
    let compiled = {
        let provider = machine
            .host
            .script_compiler()
            .ok_or_else(|| type_error("Function: this host provides no script compiler"))?;
        provider.compile_script(ScriptSource {
            source: script.as_units(),
            name: script_name.as_units(),
        })
    }
    .map_err(|error| crate::vm::compile_error(machine, error))?;
    let module = machine
        .install_script_reserving(compiled, 1, 1)
        .map_err(EvalFailure::Runtime)?;
    let entry = machine
        .allocate(HeapEntry::Function {
            module,
            function: machine.module_code(module).entry(),
            captures: Vec::new(),
            context: None,
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.function_prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    let previous = machine.context_global.take();
    let result = machine.call_value(entry, machine.global_object, &[]);
    machine.context_global = previous;
    result.map(BuiltinOutcome::Value)
}

fn type_error(operation: &'static str) -> EvalFailure {
    EvalFailure::Throw(ThrowOrigin::TypeError { operation })
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::EcmaString;
    use bamts_native::{Decoded, Value};

    use super::super::test_support::{TestHost, blank_program};
    use crate::{EvalFailure, HeapEntry, Limits, Machine, Property, PropertyKey};

    #[test]
    fn function_global_installs_with_spec_identity() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let function = machine
            .intrinsics
            .global("Function")
            .expect("Function global installs");
        let name = machine
            .get_named_property(function, "name")
            .expect("Function has a name");
        assert!(
            machine
                .to_string(name)
                .expect("name is a string")
                .eq_ascii("Function"),
            "Function.name is \"Function\""
        );
        let length = machine
            .get_named_property(function, "length")
            .expect("Function has a length");
        assert!(
            matches!(length.decode(), Some(Decoded::Int32(1))),
            "Function.length is 1"
        );
        let prototype = machine
            .get_named_property(function, "prototype")
            .expect("Function has a prototype");
        assert_eq!(
            prototype, machine.intrinsics.function_prototype,
            "Function.prototype is %FunctionPrototype%"
        );
        let constructor = machine
            .get_named_property(prototype, "constructor")
            .expect("Function.prototype has a constructor");
        assert_eq!(
            constructor, function,
            "Function.prototype.constructor is Function"
        );
    }

    #[test]
    fn function_prototype_property_is_frozen_and_non_enumerable() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let function = machine
            .intrinsics
            .global("Function")
            .expect("Function global installs");
        let key = PropertyKey::Named(EcmaString::encode("prototype"));
        let Some(Property::Data {
            value: _,
            writable,
            enumerable,
            configurable,
        }) = machine
            .own_descriptor(function, &key)
            .expect("Function has an own prototype property")
        else {
            panic!("prototype is a data property");
        };
        assert!(!writable, "Function.prototype is not writable");
        assert!(!enumerable, "Function.prototype is not enumerable");
        assert!(!configurable, "Function.prototype is not configurable");
    }

    #[test]
    fn dynamic_function_without_a_provider_throws_a_controlled_type_error() {
        let module = blank_program("<test>");
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let function = machine
            .intrinsics
            .global("Function")
            .expect("Function global installs");
        let argument = machine
            .allocate(HeapEntry::String(EcmaString::encode("return 1")))
            .expect("string allocates");
        let failure = machine
            .call_value(function, Value::UNDEFINED, &[argument])
            .expect_err("no-provider synthesis fails");
        match failure {
            EvalFailure::Throw(origin) => {
                assert!(
                    matches!(origin, crate::ThrowOrigin::TypeError { .. }),
                    "the no-provider path reports a TypeError, not a crash"
                );
            }
            other => panic!("expected a throw completion, got {other:?}"),
        }
    }
}
