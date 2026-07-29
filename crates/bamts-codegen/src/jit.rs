//! Host-native Cranelift JIT backend.

use std::error::Error;
use std::fmt;

use bamts_bytecode::{Program as BytecodeProgram, Verified};
use bamts_native::{
    AbiError, Completion, CompletionTag, JitEntry, NativeEntryTable, NativeHelper, ShadowFrame,
};
use cranelift_codegen::Context;
use cranelift_codegen::ir::{ExternalName, Function, UserExternalName};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleError, default_libcall_names};

use crate::{HELPER_NAMESPACE, Helper, LoweredProgram, ProgramLowerError, lower_program};

/// A typed host-JIT compilation failure.
#[derive(Debug)]
pub enum JitError {
    /// Backend-neutral program lowering failed.
    Lower(ProgramLowerError),
    /// Cranelift could not declare, compile, or finalize the module.
    Module(Box<ModuleError>),
    /// Lowered IR named a runtime helper outside the pinned 30-entry table.
    UnknownHelper { index: u32 },
}

impl fmt::Display for JitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JitError::Lower(error) => write!(
                formatter,
                "could not lower program for the host JIT: {error}"
            ),
            JitError::Module(error) => write!(formatter, "host JIT compilation failed: {error}"),
            JitError::UnknownHelper { index } => {
                write!(
                    formatter,
                    "lowered IR imports unknown runtime helper u1:{index}"
                )
            }
        }
    }
}

impl Error for JitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            JitError::Lower(error) => Some(error),
            JitError::Module(error) => Some(error.as_ref()),
            JitError::UnknownHelper { .. } => None,
        }
    }
}

impl From<ProgramLowerError> for JitError {
    fn from(error: ProgramLowerError) -> Self {
        JitError::Lower(error)
    }
}

impl From<ModuleError> for JitError {
    fn from(error: ModuleError) -> Self {
        JitError::Module(Box::new(error))
    }
}

/// One module-qualified native entry in tuple order.
struct JitUnit {
    module_id: u32,
    function_id: u32,
    function: FuncId,
}

/// A finalized host-native program. Its executable memory remains owned by the
/// contained [`JITModule`] and entries are callable only through
/// [`NativeEntryTable`].
pub struct JitProgram {
    module: JITModule,
    functions: Vec<JitUnit>,
    program_bytes: Vec<u8>,
    entry_module: u32,
    entry_function: u32,
}

impl JitProgram {
    /// The canonical module id used as the program entry.
    #[must_use]
    pub const fn entry_module(&self) -> u32 {
        self.entry_module
    }

    /// The bytecode function id local to the entry module.
    #[must_use]
    pub const fn entry_function(&self) -> u32 {
        self.entry_function
    }
}

impl NativeEntryTable for JitProgram {
    fn program_bytes(&self) -> &[u8] {
        &self.program_bytes
    }

    fn invoke(
        &self,
        module_id: u32,
        function_id: u32,
        frame: &mut ShadowFrame,
        out: &mut Completion,
    ) -> Result<CompletionTag, AbiError> {
        let index = self
            .functions
            .binary_search_by_key(&(module_id, function_id), |entry| {
                (entry.module_id, entry.function_id)
            })
            .map_err(|_| AbiError::UnknownFunction {
                module_id,
                function_id,
            })?;
        Ok(JitEntry::new(&self.module, self.functions[index].function).invoke(frame, out))
    }
}

/// Lowers, compiles, and finalizes every module of a verified canonical program
/// for the current host. Module-local ids remain local and native entries are
/// keyed by `(module_id, function_id)`.
pub fn compile_jit(bytecode: &BytecodeProgram<Verified>) -> Result<JitProgram, JitError> {
    let program_bytes = bytecode.encode();
    let mut builder = JITBuilder::new(default_libcall_names())?;
    bind_runtime_helpers(&mut builder);
    let module = JITModule::new(builder);
    let lowered = lower_program(bytecode, module.target_config())?;
    compile_lowered(module, lowered, program_bytes)
}

fn compile_lowered(
    mut module: JITModule,
    lowered: LoweredProgram,
    program_bytes: Vec<u8>,
) -> Result<JitProgram, JitError> {
    let function_count = lowered
        .modules
        .iter()
        .map(|module| module.functions.len())
        .sum();
    let mut functions = Vec::with_capacity(function_count);
    for lowered_module in &lowered.modules {
        for function in &lowered_module.functions {
            functions.push(JitUnit {
                module_id: lowered_module.id.get(),
                function_id: function.id.get(),
                function: module.declare_function(
                    &function.symbol,
                    Linkage::Local,
                    &function.signature,
                )?,
            });
        }
    }

    let call_conv = module.target_config().default_call_conv;
    let mut helpers = Vec::with_capacity(bamts_native::HELPER_COUNT as usize);
    for index in 0..bamts_native::HELPER_COUNT {
        let helper = Helper::from_external_index(index).ok_or(JitError::UnknownHelper { index })?;
        helpers.push(module.declare_function(
            helper.symbol(),
            Linkage::Import,
            &helper.signature(call_conv),
        )?);
    }

    let mut unit_index = 0;
    for lowered_module in lowered.modules {
        for function in lowered_module.functions {
            let mut clif = function.clif;
            rebind_helper_imports(&mut clif, &helpers)?;
            let mut context = Context::for_function(clif);
            module.define_function(functions[unit_index].function, &mut context)?;
            unit_index += 1;
        }
    }
    module.finalize_definitions()?;

    Ok(JitProgram {
        module,
        functions,
        program_bytes,
        entry_module: lowered.entry_module.get(),
        entry_function: lowered.entry_function.get(),
    })
}

fn rebind_helper_imports(clif: &mut Function, helpers: &[FuncId]) -> Result<(), JitError> {
    let imports: Vec<_> = clif
        .dfg
        .ext_funcs
        .iter()
        .filter_map(|(reference, data)| match data.name {
            ExternalName::User(name_ref) => {
                let name = &clif.params.user_named_funcs()[name_ref];
                (name.namespace == HELPER_NAMESPACE).then_some((reference, name.index))
            }
            _ => None,
        })
        .collect();

    for (reference, index) in imports {
        let function = helpers
            .get(index as usize)
            .ok_or(JitError::UnknownHelper { index })?;
        let name = clif.declare_imported_user_function(UserExternalName::new(0, function.as_u32()));
        clif.dfg.ext_funcs[reference].name = ExternalName::user(name);
    }
    Ok(())
}

fn bind_runtime_helpers(builder: &mut JITBuilder) {
    for index in 0..bamts_native::HELPER_COUNT {
        let helper = Helper::from_external_index(index).expect("pinned helper table is dense");
        builder.symbol(helper.symbol(), helper_address(helper));
    }
}

fn helper_address(helper: Helper) -> *const u8 {
    match helper {
        Helper::LoadConstant => bamts_native::bamts_load_constant as *const u8,
        Helper::Unary => bamts_native::bamts_unary as *const u8,
        Helper::Binary => bamts_native::bamts_binary as *const u8,
        Helper::CreateObject => bamts_native::bamts_create_object as *const u8,
        Helper::CreateArray => bamts_native::bamts_create_array as *const u8,
        Helper::CreateClosure => bamts_native::bamts_create_closure as *const u8,
        Helper::GetProperty => bamts_native::bamts_get_property as *const u8,
        Helper::SetProperty => bamts_native::bamts_set_property as *const u8,
        Helper::DeleteProperty => bamts_native::bamts_delete_property as *const u8,
        Helper::Call => bamts_native::bamts_call as *const u8,
        Helper::Construct => bamts_native::bamts_construct as *const u8,
        Helper::Import => bamts_native::bamts_import as *const u8,
        Helper::Truthy => bamts_native::bamts_truthy as *const u8,
        Helper::ResumeValue => bamts_native::bamts_resume_value as *const u8,
        Helper::DefineAccessor => bamts_native::bamts_define_accessor as *const u8,
        Helper::LoadGlobal => bamts_native::bamts_load_global as *const u8,
        Helper::StoreGlobal => bamts_native::bamts_store_global as *const u8,
        Helper::TypeOfGlobal => bamts_native::bamts_typeof_global as *const u8,
        Helper::LoadThis => bamts_native::bamts_load_this as *const u8,
        Helper::LoadArguments => bamts_native::bamts_load_arguments as *const u8,
        Helper::LoadNewTarget => bamts_native::bamts_load_new_target as *const u8,
        Helper::ArrayPush => bamts_native::bamts_array_push as *const u8,
        Helper::ArrayExtend => bamts_native::bamts_array_extend as *const u8,
        Helper::ObjectSpread => bamts_native::bamts_object_spread as *const u8,
        Helper::SetPrototype => bamts_native::bamts_set_prototype as *const u8,
        Helper::CreatePrivateName => bamts_native::bamts_create_private_name as *const u8,
        Helper::CreateRegExp => bamts_native::bamts_create_regexp as *const u8,
        Helper::GetIterator => bamts_native::bamts_get_iterator as *const u8,
        Helper::IteratorNext => bamts_native::bamts_iterator_next as *const u8,
        Helper::Export => bamts_native::bamts_export as *const u8,
        Helper::ConsumeFuel => bamts_native::bamts_consume_fuel as *const u8,
    }
}

const _: () = {
    let mut index = 0;
    while index < bamts_native::HELPER_COUNT {
        let helper = Helper::from_external_index(index).expect("codegen helper table is dense");
        let native = NativeHelper::from_u32(index).expect("native helper table is dense");
        assert!(helper.external_index() == native.as_u32());
        index += 1;
    }
};

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        BinaryOp, Constant, ConstantId, EcmaString, ExceptionHandler, Function as BytecodeFunction,
        FunctionFlags, FunctionId, Instruction, Module, ModuleId, Pc, Program, ProgramModule,
        Register,
    };
    use bamts_native::{
        AbiError, Completion, CompletionTag, HelperCall, HelperResult, NativeEntryTable,
        NativeFrame, NativeHelper, NativeOps, ShadowFrame, Value, with_native_ops,
    };
    use bamts_runtime::{
        Host, Limits, NativeError, RuntimeError, RuntimeErrorKind, run_linked_program,
    };

    use crate::Helper;

    use super::compile_jit;

    struct SilentHost;

    impl Host for SilentHost {}

    struct FatalResume;

    impl NativeOps for FatalResume {
        fn truthy(&self, _frame: &mut NativeFrame<'_>, _value: Value) -> bool {
            unreachable!("resume fixture never tests truthiness")
        }

        fn dispatch(&self, _frame: &mut NativeFrame<'_>, call: HelperCall) -> HelperResult {
            assert_eq!(call, HelperCall::ResumeValue);
            HelperResult {
                tag: CompletionTag::FatalTrap,
                value: Value::int32(99),
            }
        }
    }

    #[derive(Default)]
    struct RecordingHost {
        stdout: Vec<u8>,
    }

    impl Host for RecordingHost {
        fn write_stdout(&mut self, bytes: &[u8]) {
            self.stdout.extend_from_slice(bytes);
        }
    }

    fn module(name: &str) -> ProgramModule<bamts_bytecode::Verified> {
        ProgramModule {
            name: ConstantId::new(0),
            code: Module::new(
                vec![Constant::String(EcmaString::from_utf8(name))],
                vec![BytecodeFunction::new(
                    None,
                    0,
                    0,
                    0,
                    FunctionFlags::default(),
                    vec![Instruction::Halt],
                    Vec::new(),
                )],
                FunctionId::new(0),
            )
            .verify()
            .expect("test module verifies"),
            edges: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
        }
    }

    fn two_module_program() -> Program<bamts_bytecode::Verified> {
        Program::link(vec![module("first"), module("entry")], ModuleId::new(1))
            .expect("test program verifies")
    }

    fn one_function_program(
        constants: Vec<Constant>,
        register_count: u32,
        code: Vec<Instruction>,
        handlers: Vec<ExceptionHandler>,
    ) -> Program<bamts_bytecode::Verified> {
        let module = ProgramModule {
            name: ConstantId::new(0),
            code: Module::new(
                constants,
                vec![BytecodeFunction::new(
                    None,
                    0,
                    0,
                    register_count,
                    FunctionFlags::default(),
                    code,
                    handlers,
                )],
                FunctionId::new(0),
            )
            .verify()
            .expect("metering fixture verifies"),
            edges: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
        };
        Program::link(vec![module], ModuleId::new(0)).expect("metering fixture links")
    }

    fn callback_reentry_program() -> Program<bamts_bytecode::Verified> {
        let constants = vec![
            Constant::String(EcmaString::from_utf8("entry")),
            Constant::String(EcmaString::from_utf8("Array")),
            Constant::String(EcmaString::from_utf8("prototype")),
            Constant::String(EcmaString::from_utf8("map")),
            Constant::Int32(1),
        ];
        let entry = BytecodeFunction::new(
            None,
            0,
            0,
            11,
            FunctionFlags::default(),
            vec![
                Instruction::LoadGlobal {
                    dst: Register::new(0),
                    name: ConstantId::new(1),
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
                Instruction::GetProperty {
                    dst: Register::new(4),
                    object: Register::new(2),
                    key: Register::new(3),
                },
                Instruction::CreateArray {
                    dst: Register::new(5),
                },
                Instruction::LoadConst {
                    dst: Register::new(6),
                    constant: ConstantId::new(4),
                },
                Instruction::ArrayPush {
                    array: Register::new(5),
                    value: Register::new(6),
                },
                Instruction::CreateArray {
                    dst: Register::new(7),
                },
                Instruction::CreateClosure {
                    dst: Register::new(8),
                    function: FunctionId::new(1),
                    captures: Register::new(7),
                },
                Instruction::CreateArray {
                    dst: Register::new(9),
                },
                Instruction::ArrayPush {
                    array: Register::new(9),
                    value: Register::new(8),
                },
                Instruction::Call {
                    dst: Register::new(10),
                    callee: Register::new(4),
                    this_value: Register::new(5),
                    arguments: Register::new(9),
                },
                Instruction::Halt,
            ],
            Vec::new(),
        );
        let callback = BytecodeFunction::new(
            None,
            0,
            0,
            1,
            FunctionFlags::default(),
            vec![Instruction::Halt],
            Vec::new(),
        );
        let module = ProgramModule {
            name: ConstantId::new(0),
            code: Module::new(constants, vec![entry, callback], FunctionId::new(0))
                .verify()
                .expect("callback fixture verifies"),
            edges: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
        };
        Program::link(vec![module], ModuleId::new(0)).expect("callback fixture links")
    }

    fn assert_fuel_exhausted(
        result: Result<bamts_runtime::ExecutionOutcome, NativeError>,
        limit: u64,
    ) -> RuntimeError {
        let Err(NativeError::Runtime(error)) = result else {
            panic!("expected fuel exhaustion at limit {limit}, got {result:?}");
        };
        assert_eq!(error.kind, RuntimeErrorKind::FuelExhausted { limit });
        error
    }

    #[test]
    fn codegen_and_native_helper_tables_are_identical() {
        for index in 0..bamts_native::HELPER_COUNT {
            let helper = Helper::from_external_index(index).expect("codegen helper exists");
            let native = NativeHelper::from_u32(index).expect("native helper exists");
            assert_eq!(helper.external_index(), native.as_u32());
            assert_eq!(helper.symbol(), native.symbol());
        }
    }

    #[test]
    fn compiles_duplicate_local_function_ids_and_reports_entry_tuple() {
        let program = compile_jit(&two_module_program()).expect("host JIT compiles every module");
        assert_eq!(program.entry_module(), 1);
        assert_eq!(program.entry_function(), 0);

        for module_id in [0, 1] {
            let mut register = Value::UNINITIALIZED;
            let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, module_id, &mut register, 1);
            let mut out = Completion::new(Value::UNDEFINED);
            // Direct entry invocation has no NativeOps dispatcher installed, so
            // the mandatory pre-Halt fuel helper reports a fatal trap. Reaching
            // it proves both local function-id tuples were compiled and bound.
            assert_eq!(
                program.invoke(module_id, 0, &mut frame, &mut out),
                Ok(CompletionTag::FatalTrap)
            );
        }
    }

    #[test]
    fn matched_jit_program_runs_through_linked_runtime() {
        let bytecode = two_module_program();
        let program = compile_jit(&bytecode).expect("host JIT compiles every module");
        assert_eq!(program.program_bytes(), bytecode.encode());

        let mut host = SilentHost;
        let outcome = run_linked_program(&bytecode, &program, &mut host, &Limits::default())
            .expect("matching JIT program runs");
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn jit_charges_each_mixed_instruction_once_at_exact_boundaries() {
        let bytecode = one_function_program(
            vec![
                Constant::String(EcmaString::from_utf8("entry")),
                Constant::Int32(1),
            ],
            3,
            vec![
                Instruction::LoadConst {
                    dst: Register::new(0),
                    constant: ConstantId::new(1),
                },
                Instruction::Move {
                    dst: Register::new(1),
                    src: Register::new(0),
                },
                Instruction::Binary {
                    dst: Register::new(2),
                    op: BinaryOp::Add,
                    left: Register::new(0),
                    right: Register::new(1),
                },
                Instruction::Jump { target: Pc::new(4) },
                Instruction::Halt,
            ],
            Vec::new(),
        );
        let compiled = compile_jit(&bytecode).expect("mixed program compiles");

        for (fuel, pc) in [(0, 0), (4, 4)] {
            let mut host = SilentHost;
            let result = run_linked_program(
                &bytecode,
                &compiled,
                &mut host,
                &Limits {
                    fuel,
                    ..Limits::default()
                },
            );
            assert_eq!(assert_fuel_exhausted(result, fuel).pc, Pc::new(pc));
        }

        let mut host = SilentHost;
        let result = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                fuel: 5,
                ..Limits::default()
            },
        );
        assert!(
            result.is_ok(),
            "five instructions fit fuel five: {result:?}"
        );
    }

    #[test]
    fn resumed_helper_failure_reports_suspend_pc() {
        let bytecode = one_function_program(
            vec![
                Constant::String(EcmaString::from_utf8("entry")),
                Constant::Null,
            ],
            1,
            vec![
                Instruction::LoadConst {
                    dst: Register::new(0),
                    constant: ConstantId::new(1),
                },
                Instruction::Suspend {
                    dst: Register::new(0),
                    src: Register::new(0),
                    resume: Pc::new(2),
                },
                Instruction::Halt,
            ],
            Vec::new(),
        );
        let program = compile_jit(&bytecode).expect("suspend program compiles");
        let mut register = Value::UNINITIALIZED;
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 2, 0, &mut register, 1);
        let mut completion = Completion::new(Value::UNDEFINED);
        let mut ops = FatalResume;

        let tag = with_native_ops(&mut ops, || {
            program.invoke(0, 0, &mut frame, &mut completion)
        })
        .expect("compiled entry is registered");

        assert_eq!(tag, CompletionTag::FatalTrap);
        assert_eq!(frame.bytecode_pc, 1);
    }

    #[test]
    fn jit_jump_to_self_exhausts_fuel() {
        let bytecode = one_function_program(
            vec![Constant::String(EcmaString::from_utf8("entry"))],
            0,
            vec![Instruction::Jump { target: Pc::new(0) }],
            Vec::new(),
        );
        let compiled = compile_jit(&bytecode).expect("spin loop compiles");
        let mut host = SilentHost;
        let result = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                fuel: 3,
                ..Limits::default()
            },
        );
        assert_fuel_exhausted(result, 3);
    }

    #[test]
    fn jit_builtin_callback_reentry_shares_all_machine_budgets() {
        let bytecode = callback_reentry_program();
        let compiled = compile_jit(&bytecode).expect("callback program compiles");

        let mut host = SilentHost;
        let exact = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                fuel: 15,
                max_call_depth: 2,
                max_total_registers: 12,
                ..Limits::default()
            },
        );
        assert!(
            exact.is_ok(),
            "entry plus callback fit exact shared limits: {exact:?}"
        );

        let mut host = SilentHost;
        assert_fuel_exhausted(
            run_linked_program(
                &bytecode,
                &compiled,
                &mut host,
                &Limits {
                    fuel: 14,
                    max_call_depth: 2,
                    max_total_registers: 12,
                    ..Limits::default()
                },
            ),
            14,
        );

        let mut host = SilentHost;
        let depth = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                max_call_depth: 1,
                max_total_registers: 12,
                ..Limits::default()
            },
        );
        assert!(matches!(
            depth,
            Err(NativeError::Runtime(RuntimeError {
                kind: RuntimeErrorKind::CallDepthExceeded { limit: 1 },
                ..
            }))
        ));

        let mut host = SilentHost;
        let registers = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                max_call_depth: 2,
                max_total_registers: 11,
                ..Limits::default()
            },
        );
        assert!(matches!(
            registers,
            Err(NativeError::Runtime(RuntimeError {
                kind: RuntimeErrorKind::RegisterLimitExceeded { limit: 11 },
                ..
            }))
        ));
    }

    #[test]
    fn jit_fuel_exhaustion_bypasses_bytecode_handler() {
        let bytecode = one_function_program(
            vec![
                Constant::String(EcmaString::from_utf8("entry")),
                Constant::Undefined,
            ],
            1,
            vec![
                Instruction::LoadConst {
                    dst: Register::new(0),
                    constant: ConstantId::new(1),
                },
                Instruction::Throw {
                    value: Register::new(0),
                },
                Instruction::Halt,
            ],
            vec![ExceptionHandler {
                start: Pc::new(1),
                end: Pc::new(2),
                handler: Pc::new(2),
                catch_register: Register::new(0),
            }],
        );
        let compiled = compile_jit(&bytecode).expect("handler program compiles");
        let mut host = SilentHost;
        let result = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                fuel: 1,
                ..Limits::default()
            },
        );
        assert_fuel_exhausted(result, 1);
    }

    #[test]
    fn jit_zero_fuel_prevents_stdout_side_effects() {
        let bytecode = one_function_program(
            vec![
                Constant::String(EcmaString::from_utf8("entry")),
                Constant::String(EcmaString::from_utf8("console")),
                Constant::String(EcmaString::from_utf8("log")),
                Constant::String(EcmaString::from_utf8("hello")),
            ],
            6,
            vec![
                Instruction::LoadGlobal {
                    dst: Register::new(0),
                    name: ConstantId::new(1),
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
                Instruction::CreateArray {
                    dst: Register::new(4),
                },
                Instruction::ArrayPush {
                    array: Register::new(4),
                    value: Register::new(3),
                },
                Instruction::Call {
                    dst: Register::new(5),
                    callee: Register::new(2),
                    this_value: Register::new(0),
                    arguments: Register::new(4),
                },
                Instruction::Halt,
            ],
            Vec::new(),
        );
        let compiled = compile_jit(&bytecode).expect("stdout fixture compiles");
        let mut host = RecordingHost::default();
        let result = run_linked_program(
            &bytecode,
            &compiled,
            &mut host,
            &Limits {
                fuel: 0,
                ..Limits::default()
            },
        );
        assert_fuel_exhausted(result, 0);
        assert!(host.stdout.is_empty(), "fuel failure precedes console.log");
    }

    #[test]
    fn unknown_module_and_function_tuples_are_rejected() {
        let program = compile_jit(&two_module_program()).expect("host JIT compiles");
        let mut register = Value::UNINITIALIZED;
        let mut frame = ShadowFrame::new(core::ptr::null_mut(), 0, 0, &mut register, 1);
        let mut out = Completion::new(Value::UNDEFINED);

        for (module_id, function_id) in [(7, 0), (0, 7)] {
            assert_eq!(
                program.invoke(module_id, function_id, &mut frame, &mut out),
                Err(AbiError::UnknownFunction {
                    module_id,
                    function_id,
                })
            );
        }
    }
}
