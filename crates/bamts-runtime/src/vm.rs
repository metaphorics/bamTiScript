use std::collections::BTreeMap;

use bamts_bytecode::EcmaString;
use bamts_native::Value;

use crate::external_modules::InstalledModule;
use crate::intrinsics::{self, BuiltinDef, BuiltinOutcome, BuiltinTable};
use crate::{
    EvalFailure, HeapEntry, Host, Machine, PropertyMap, ScriptCompileError, ScriptSource,
    ThrowOrigin,
};

pub(crate) fn install<H: Host>(
    heap: &mut Vec<HeapEntry>,
    builtins: &mut BuiltinTable<H>,
    object_prototype: Value,
) -> InstalledModule {
    let specifier = EcmaString::from_utf8("node:vm");
    let namespace = intrinsics::push(
        heap,
        HeapEntry::ExternalModuleNamespace {
            specifier: specifier.clone(),
        },
    );
    let script_prototype = intrinsics::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(object_prototype),
            boxed_primitive: None,
            extensible: true,
        },
    );
    let script = register(heap, builtins, "Script", 1, script_constructor::<H>);
    let run_in_this_context = register(
        heap,
        builtins,
        "runInThisContext",
        2,
        run_in_this_context::<H>,
    );
    let run_prototype = intrinsics::push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(object_prototype),
            boxed_primitive: None,
            extensible: true,
        },
    );
    builtins.set_function_prototype(heap, run_in_this_context, run_prototype);
    crate::intrinsics::builtins::define_data(
        heap,
        run_prototype,
        "constructor",
        run_in_this_context,
    );
    let script_run_in_this_context = register(
        heap,
        builtins,
        "runInThisContext",
        1,
        script_run_in_this_context::<H>,
    );

    builtins.set_constructor_prototype(heap, script, script_prototype);
    crate::intrinsics::builtins::define_data(heap, script_prototype, "constructor", script);
    crate::intrinsics::builtins::define_data(
        heap,
        script_prototype,
        "runInThisContext",
        script_run_in_this_context,
    );

    InstalledModule {
        specifier,
        namespace,
        exports: vec![
            (EcmaString::from_utf8("Script"), script),
            (
                EcmaString::from_utf8("runInThisContext"),
                run_in_this_context,
            ),
        ],
        internals: BTreeMap::from([("vm.script.prototype", script_prototype)]),
    }
}

fn register<H: Host>(
    heap: &mut Vec<HeapEntry>,
    builtins: &mut BuiltinTable<H>,
    name: &'static str,
    length: u32,
    handler: intrinsics::BuiltinHandler<H>,
) -> Value {
    let id = builtins.register(BuiltinDef {
        name,
        length,
        handler,
    });
    intrinsics::native_function(heap, id, name, length)
}

fn type_error(operation: &'static str) -> EvalFailure {
    EvalFailure::Throw(ThrowOrigin::TypeError { operation })
}

#[derive(Clone, Copy)]
enum ScriptAllocation {
    Call,
    ConstructedCall,
    Object,
}

impl ScriptAllocation {
    fn wrappers(self) -> usize {
        match self {
            Self::Call => 1,
            Self::ConstructedCall | Self::Object => 2,
        }
    }
}

fn script_constructor<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if !constructing {
        return Err(type_error(
            "Class constructor Script cannot be invoked without 'new'",
        ));
    }

    let (code, name) = source_arguments(machine, args)?;
    let entry = compile_entry(machine, code, name, ScriptAllocation::Object)?;
    let script = machine
        .allocate(HeapEntry::Script {
            entry,
            properties: PropertyMap::default(),
            prototype: Some(script_prototype(machine)),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(BuiltinOutcome::Value(script))
}

fn run_in_this_context<H: Host>(
    machine: &mut Machine<'_, H>,
    _this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    let prototype = if constructing {
        let function = machine
            .registry
            .external
            .get(&EcmaString::from_utf8("node:vm"))
            .and_then(|module| {
                module
                    .exports
                    .get(&EcmaString::from_utf8("runInThisContext"))
            })
            .expect("node:vm installs runInThisContext")
            .value;
        Some(
            machine
                .constructed_prototype(function)
                .map_err(EvalFailure::Runtime)?,
        )
    } else {
        None
    };
    let (code, name) = source_arguments(machine, args)?;
    let allocation = if constructing {
        ScriptAllocation::ConstructedCall
    } else {
        ScriptAllocation::Call
    };
    let entry = compile_entry(machine, code, name, allocation)?;
    call_entry(machine, entry, args.len(), prototype)
}

fn script_run_in_this_context<H: Host>(
    machine: &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure> {
    if constructing {
        return Err(type_error(
            "Script.prototype.runInThisContext is not a constructor",
        ));
    }
    if let Some(options) = args
        .first()
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
        && (!machine.is_object(options) || machine.is_callable(options)?)
    {
        return Err(type_error("The \"options\" argument must be an object"));
    }
    let Some(index) = machine.runtime_slot(this).map_err(EvalFailure::Runtime)? else {
        return Err(type_error(
            "Script.prototype.runInThisContext called on incompatible receiver",
        ));
    };
    let HeapEntry::Script { entry, .. } = &machine.heap[index] else {
        return Err(type_error(
            "Script.prototype.runInThisContext called on incompatible receiver",
        ));
    };
    call_entry(machine, *entry, args.len(), None)
}

fn source_arguments<H: Host>(
    machine: &mut Machine<'_, H>,
    args: &[Value],
) -> Result<(EcmaString, EcmaString), EvalFailure> {
    let code = machine.to_string(args.first().copied().unwrap_or(Value::UNDEFINED))?;
    let Some(options) = args
        .get(1)
        .copied()
        .filter(|value| *value != Value::UNDEFINED)
    else {
        return Ok((code, EcmaString::from_utf8("evalmachine.<anonymous>")));
    };
    if let Some(name) = machine.string_value(options) {
        return Ok((code, name));
    }
    if !machine.is_object(options) || machine.is_callable(options)? {
        return Err(type_error("The \"options\" argument must be an object"));
    }
    let filename = machine.get_named_property(options, "filename")?;
    if filename == Value::UNDEFINED {
        return Ok((code, EcmaString::from_utf8("evalmachine.<anonymous>")));
    }
    let Some(name) = machine.string_value(filename) else {
        return Err(type_error("The \"filename\" option must be of type string"));
    };
    Ok((code, name))
}

fn compile_entry<H: Host>(
    machine: &mut Machine<'_, H>,
    code: EcmaString,
    name: EcmaString,
    allocation: ScriptAllocation,
) -> Result<Value, EvalFailure> {
    let compiled = {
        let provider = machine
            .host
            .script_compiler()
            .ok_or_else(|| type_error("host withdrew its script compiler"))?;
        provider.compile_script(ScriptSource {
            source: code.as_units(),
            name: name.as_units(),
        })
    }
    .map_err(|error| compile_error(machine, error))?;
    let wrappers = allocation.wrappers();
    let module = machine
        .install_script_reserving(compiled, wrappers, wrappers)
        .map_err(EvalFailure::Runtime)?;
    let entry = machine
        .allocate(HeapEntry::Function {
            module,
            function: machine.module_code(module).entry(),
            captures: Vec::new(),
            properties: PropertyMap::default(),
            prototype: Some(machine.intrinsics.function_prototype),
            extensible: true,
        })
        .map_err(EvalFailure::Runtime)?;
    Ok(entry)
}

fn compile_error<H: Host>(machine: &mut Machine<'_, H>, error: ScriptCompileError) -> EvalFailure {
    let (kind, message) = match error {
        ScriptCompileError::IllFormedSource { unit_offset } => (
            "SyntaxError",
            format!("ill-formed UTF-16 source at code unit {unit_offset}"),
        ),
        ScriptCompileError::Syntax { message, .. }
        | ScriptCompileError::Unsupported { message, .. } => ("SyntaxError", message),
        ScriptCompileError::Capacity { message } => ("RangeError", message),
    };
    let id = machine
        .intrinsics
        .builtins
        .id_named(kind)
        .expect("error constructors install before external modules");
    machine.throw_error(id, message)
}

fn script_prototype<H: Host>(machine: &Machine<'_, H>) -> Value {
    *machine
        .registry
        .external
        .get(&EcmaString::from_utf8("node:vm"))
        .and_then(|module| module.internals.get("vm.script.prototype"))
        .expect("node:vm installs Script.prototype")
}

fn call_entry<H: Host>(
    machine: &Machine<'_, H>,
    entry: Value,
    argument_start: usize,
    prototype: Option<Value>,
) -> Result<BuiltinOutcome, EvalFailure> {
    let this_value = machine
        .intrinsics
        .global("globalThis")
        .expect("host objects install globalThis");
    Ok(match prototype {
        Some(prototype) => BuiltinOutcome::ConstructCall {
            callee: entry,
            this_value,
            argument_start,
            prototype,
        },
        None => BuiltinOutcome::Call {
            callee: entry,
            this_value,
            argument_start,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Register, Verified,
    };

    use super::*;
    use crate::{CompileProvider, Limits, RuntimeErrorKind};

    struct FakeCompiler {
        program: Arc<Program<Verified>>,
        sources: Vec<(Vec<u16>, Vec<u16>)>,
    }

    impl CompileProvider for FakeCompiler {
        fn compile_script(
            &mut self,
            source: ScriptSource<'_>,
        ) -> Result<Arc<Program<Verified>>, ScriptCompileError> {
            self.sources
                .push((source.source.to_vec(), source.name.to_vec()));
            Ok(self.program.clone())
        }
    }

    #[derive(Default)]
    struct ScriptHost {
        compiler: Option<FakeCompiler>,
    }

    impl Host for ScriptHost {
        fn script_compiler(&mut self) -> Option<&mut (dyn CompileProvider + 'static)> {
            self.compiler
                .as_mut()
                .map(|compiler| compiler as &mut (dyn CompileProvider + 'static))
        }
    }

    fn program_returning(value: i32) -> Program<Verified> {
        let constants = vec![
            Constant::Int32(value),
            Constant::String(EcmaString::from_utf8("test")),
        ];
        let code = Module::new(
            constants,
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![
                    Instruction::LoadConst {
                        dst: Register::new(0),
                        constant: ConstantId::new(0),
                    },
                    Instruction::Return {
                        value: Register::new(0),
                    },
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("test bytecode is verified");
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(1),
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("test program links")
    }

    fn compiler_host() -> ScriptHost {
        ScriptHost {
            compiler: Some(FakeCompiler {
                program: Arc::new(program_returning(42)),
                sources: Vec::new(),
            }),
        }
    }

    fn vm_exports<H: Host>(machine: &Machine<'_, H>) -> (Value, Value) {
        let vm = machine
            .registry
            .external
            .get(&EcmaString::from_utf8("node:vm"))
            .expect("node:vm is installed");
        (
            vm.exports
                .get(&EcmaString::from_utf8("Script"))
                .expect("Script is exported")
                .value,
            vm.exports
                .get(&EcmaString::from_utf8("runInThisContext"))
                .expect("runInThisContext is exported")
                .value,
        )
    }

    fn builtin_id<H: Host>(machine: &Machine<'_, H>, value: Value) -> crate::intrinsics::BuiltinId {
        let index = machine
            .runtime_slot(value)
            .expect("builtin value is valid")
            .expect("builtin is a runtime object");
        let HeapEntry::NativeFunction { id, .. } = &machine.heap[index] else {
            panic!("module export is a native function");
        };
        *id
    }

    #[test]
    fn node_vm_is_absent_without_compile_capability() {
        let program = program_returning(0);
        let mut host = ScriptHost::default();
        let machine = Machine::new(&program, &mut host, Limits::default());
        assert!(
            !machine
                .registry
                .external
                .contains_key(&EcmaString::from_utf8("node:vm"))
        );
    }

    #[test]
    fn node_vm_exports_only_the_bounded_surface() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        let vm = machine
            .registry
            .external
            .get(&EcmaString::from_utf8("node:vm"))
            .expect("node:vm is installed");
        let names: Vec<_> = vm.exports.keys().cloned().collect();
        assert_eq!(
            names,
            vec![
                EcmaString::from_utf8("Script"),
                EcmaString::from_utf8("default"),
                EcmaString::from_utf8("runInThisContext"),
            ]
        );
        let (script, run) = vm_exports(&machine);
        let prototype = script_prototype(&machine);
        assert_eq!(
            machine
                .get_named_property(prototype, "constructor")
                .unwrap(),
            script
        );
        let prototype_index = machine.runtime_slot(prototype).unwrap().unwrap();
        let HeapEntry::Object { properties, .. } = &machine.heap[prototype_index] else {
            unreachable!();
        };
        assert!(matches!(
            properties.get_ascii("constructor"),
            Some(crate::Property::Data {
                value,
                writable: true,
                enumerable: false,
                configurable: true,
            }) if *value == script
        ));
        let script_index = machine.runtime_slot(script).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            properties: script_properties,
            ..
        } = &machine.heap[script_index]
        else {
            unreachable!();
        };
        assert!(matches!(
            script_properties.get_ascii("prototype"),
            Some(crate::Property::Data {
                value,
                writable: false,
                enumerable: false,
                configurable: false,
            }) if *value == prototype
        ));
        let run_index = machine.runtime_slot(run).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            properties: run_properties,
            ..
        } = &machine.heap[run_index]
        else {
            unreachable!();
        };
        let Some(crate::Property::Data {
            value: run_prototype,
            writable: true,
            enumerable: false,
            configurable: false,
        }) = run_properties.get_ascii("prototype")
        else {
            panic!("runInThisContext has an ordinary function prototype");
        };
        assert_eq!(
            machine
                .get_named_property(*run_prototype, "constructor")
                .unwrap(),
            run
        );
        assert_eq!(
            machine
                .intrinsics
                .builtins
                .get(builtin_id(&machine, script))
                .length,
            1
        );
        assert_eq!(
            machine
                .intrinsics
                .builtins
                .get(builtin_id(&machine, run))
                .length,
            2
        );
    }

    #[test]
    fn script_constructs_without_execution_and_runs_via_continuation() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (script_constructor, _) = vm_exports(&machine);
        let source = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("ignored")))
            .expect("source allocation succeeds");
        let script_constructor_id = builtin_id(&machine, script_constructor);
        let script = match machine
            .call_builtin(script_constructor_id, Value::UNDEFINED, &[source], true)
            .expect("construction succeeds")
        {
            BuiltinOutcome::Value(script) => script,
            BuiltinOutcome::Call { .. } | BuiltinOutcome::ConstructCall { .. } => {
                panic!("Script construction must not execute")
            }
        };
        let run = machine
            .get_named_property(script, "runInThisContext")
            .expect("Script prototype method is readable");
        assert_eq!(
            machine.call_value(run, script, &[]).unwrap(),
            Value::int32(42)
        );
        assert!(matches!(
            machine.call_value(run, script, &[Value::NULL]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert!(matches!(
            machine.call_value(run, script, &[run]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert!(matches!(
            machine.call_value(run, script, &[source]),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert!(matches!(
            machine.call_builtin(
                script_constructor_id,
                Value::UNDEFINED,
                &[source, run],
                true,
            ),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        assert!(matches!(
            machine.call_builtin(builtin_id(&machine, run), script, &[], true),
            Err(EvalFailure::Throw(ThrowOrigin::TypeError { .. }))
        ));
        let compiler = machine
            .host
            .compiler
            .as_ref()
            .expect("compiler remains installed");
        assert_eq!(compiler.sources.len(), 1);
        assert_eq!(
            compiler.sources[0].0,
            "ignored".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            compiler.sources[0].1,
            "evalmachine.<anonymous>".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn code_is_to_string_coerced_but_options_and_filename_are_typed() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (_, run) = vm_exports(&machine);
        assert_eq!(
            machine
                .call_value(run, Value::UNDEFINED, &[Value::int32(1)])
                .unwrap(),
            Value::int32(42)
        );
        let compiler = machine
            .host
            .compiler
            .as_ref()
            .expect("compiler remains installed");
        assert_eq!(
            compiler.sources[0].0,
            "1".encode_utf16().collect::<Vec<_>>()
        );
        let filename = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("custom.js")))
            .unwrap();
        assert_eq!(
            machine
                .call_value(run, Value::UNDEFINED, &[Value::int32(1), filename])
                .unwrap(),
            Value::int32(42)
        );
        assert_eq!(
            machine.host.compiler.as_ref().unwrap().sources[1].1,
            "custom.js".encode_utf16().collect::<Vec<_>>()
        );
        let error = machine
            .call_value(run, Value::UNDEFINED, &[Value::int32(1), run])
            .expect_err("callable options are rejected");
        assert!(matches!(
            error,
            EvalFailure::Throw(ThrowOrigin::TypeError { .. })
        ));
        let error = machine
            .call_value(run, Value::UNDEFINED, &[Value::int32(1), Value::NULL])
            .expect_err("null options are rejected");
        assert!(matches!(
            error,
            EvalFailure::Throw(ThrowOrigin::TypeError { .. })
        ));
        let options = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                boxed_primitive: None,
                extensible: true,
            })
            .expect("options allocation succeeds");
        machine
            .set_data_property(options, "filename", Value::NULL)
            .expect("options property is writable");
        let error = machine
            .call_value(run, Value::UNDEFINED, &[Value::int32(1), options])
            .expect_err("non-string filename is rejected");
        assert!(matches!(
            error,
            EvalFailure::Throw(ThrowOrigin::TypeError { .. })
        ));
    }

    #[test]
    fn constructed_run_preflight_does_not_retain_failed_installation() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (_, run) = vm_exports(&machine);
        let source = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("ignored")))
            .unwrap();
        let used_slots = machine.heap.len() - machine.intrinsic_slots;
        machine.limits.max_heap_slots = used_slots + 1;
        let before = (
            machine.dynamic.len(),
            machine.registry.modules.len(),
            machine.heap.len(),
            machine.heap_bytes,
        );

        let error = machine
            .call_builtin(builtin_id(&machine, run), Value::UNDEFINED, &[source], true)
            .expect_err("entry and fallback receiver exceed the slot budget");

        assert!(matches!(
            error,
            EvalFailure::Runtime(RuntimeErrorKind::HeapSlotLimitExceeded { .. })
        ));
        assert_eq!(
            (
                machine.dynamic.len(),
                machine.registry.modules.len(),
                machine.heap.len(),
                machine.heap_bytes,
            ),
            before
        );
    }

    #[test]
    fn script_wrapper_preflight_does_not_retain_failed_installation() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (script_constructor, _) = vm_exports(&machine);
        let source = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("ignored")))
            .unwrap();
        let used_slots = machine.heap.len() - machine.intrinsic_slots;
        machine.limits.max_heap_slots = used_slots + 1;
        let before = (
            machine.dynamic.len(),
            machine.registry.modules.len(),
            machine.heap.len(),
            machine.heap_bytes,
        );

        let error = machine
            .call_builtin(
                builtin_id(&machine, script_constructor),
                Value::UNDEFINED,
                &[source],
                true,
            )
            .expect_err("Script wrappers exceed the remaining slot budget");

        assert!(matches!(
            error,
            EvalFailure::Runtime(RuntimeErrorKind::HeapSlotLimitExceeded { .. })
        ));
        assert_eq!(
            (
                machine.dynamic.len(),
                machine.registry.modules.len(),
                machine.heap.len(),
                machine.heap_bytes,
            ),
            before
        );
    }

    #[test]
    fn script_wrapper_byte_preflight_does_not_retain_failed_installation() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (script_constructor, _) = vm_exports(&machine);
        let source = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("ignored")))
            .unwrap();
        let script_bytes = Machine::<ScriptHost>::script_heap_cost(
            &machine.host.compiler.as_ref().unwrap().program,
        );
        machine.limits.max_heap_bytes = machine.heap_bytes + script_bytes + 1;
        let before = (
            machine.dynamic.len(),
            machine.registry.modules.len(),
            machine.heap.len(),
            machine.heap_bytes,
        );

        let error = machine
            .call_builtin(
                builtin_id(&machine, script_constructor),
                Value::UNDEFINED,
                &[source],
                true,
            )
            .expect_err("Script wrappers exceed the remaining byte budget");

        assert!(matches!(
            error,
            EvalFailure::Runtime(RuntimeErrorKind::HeapByteLimitExceeded { .. })
        ));
        assert_eq!(
            (
                machine.dynamic.len(),
                machine.registry.modules.len(),
                machine.heap.len(),
                machine.heap_bytes,
            ),
            before
        );
    }

    #[test]
    fn script_receiver_cannot_be_forged() {
        let program = program_returning(0);
        let mut host = compiler_host();
        let mut machine = Machine::new(&program, &mut host, Limits::default());
        machine.instantiate_modules().unwrap();
        let (script_constructor, _) = vm_exports(&machine);
        let source = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("ignored")))
            .expect("source allocation succeeds");
        let script_constructor_id = builtin_id(&machine, script_constructor);
        let script = match machine
            .call_builtin(script_constructor_id, Value::UNDEFINED, &[source], true)
            .unwrap()
        {
            BuiltinOutcome::Value(script) => script,
            BuiltinOutcome::Call { .. } | BuiltinOutcome::ConstructCall { .. } => unreachable!(),
        };
        let run = machine
            .get_named_property(script, "runInThisContext")
            .unwrap();
        let error = machine
            .call_value(run, Value::UNDEFINED, &[])
            .expect_err("foreign receiver is rejected");
        assert!(matches!(
            error,
            EvalFailure::Throw(ThrowOrigin::TypeError { .. })
        ));
        let index = machine.runtime_slot(script).unwrap().unwrap();
        let HeapEntry::Script { properties, .. } = &machine.heap[index] else {
            unreachable!();
        };
        assert_eq!(properties.iter().count(), 0);
    }
}
