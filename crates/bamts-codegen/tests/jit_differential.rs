#![cfg(feature = "host-jit")]

use std::collections::BTreeSet;

use bamts_bytecode::{
    BinaryOp, Constant, ConstantId, EcmaString, Function, FunctionFlags, FunctionId, Instruction,
    Module, ModuleId, Pc, Program, ProgramModule, Register, Verified,
};
use bamts_codegen::compile_jit;
use bamts_codegen::helpers::{
    HelperDemand, ISA_REPRESENTATIVES, STRUCTURAL_HELPERS, helper_demand, verify_helper_coverage,
};
use bamts_codegen::tiering::{
    DeoptReason, OsrEntry, Tier, TierTransition, TieringState, WarmupPolicy,
    verify_tiering_contract,
};
use bamts_native::NativeEntryTable;
use bamts_runtime::{
    Host, Limits, NativeError, RuntimeError, run as run_interpreter, run_linked_program,
};

#[derive(Default)]
struct RecordingHost {
    stdout: Vec<u8>,
}

impl Host for RecordingHost {
    fn write_stdout(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Observable {
    Normal { stdout: Vec<u8>, exit_code: i32 },
    Abrupt(RuntimeError),
}

fn function(register_count: u32, code: Vec<Instruction>) -> Function {
    Function::new(
        None,
        0,
        0,
        register_count,
        FunctionFlags::default(),
        code,
        Vec::new(),
    )
}

fn program(
    constants: Vec<Constant>,
    register_count: u32,
    code: Vec<Instruction>,
) -> Program<Verified> {
    let module = ProgramModule {
        name: ConstantId::new(0),
        code: Module::new(
            constants,
            vec![function(register_count, code)],
            FunctionId::new(0),
        )
        .verify()
        .expect("stress fixture must verify"),
        edges: Vec::new(),
        bindings: Vec::new(),
        exports: Vec::new(),
    };
    Program::link(vec![module], ModuleId::new(0)).expect("stress fixture must link")
}

fn interpreter(program: &Program<Verified>, limits: &Limits) -> Observable {
    let mut host = RecordingHost::default();
    match run_interpreter(program, &mut host, limits) {
        Ok(outcome) => Observable::Normal {
            stdout: host.stdout,
            exit_code: outcome.exit_code,
        },
        Err(error) => Observable::Abrupt(error),
    }
}

fn jit(program: &Program<Verified>, limits: &Limits) -> Observable {
    let compiled = compile_jit(program).expect("stress fixture must compile to native code");
    assert_eq!(
        compiled.program_bytes(),
        program.encode(),
        "a published JIT artifact must remain bound to the exact verified program",
    );
    let mut host = RecordingHost::default();
    match run_linked_program(program, &compiled, &mut host, limits) {
        Ok(outcome) => Observable::Normal {
            stdout: host.stdout,
            exit_code: outcome.exit_code,
        },
        Err(NativeError::Runtime(error)) => Observable::Abrupt(error),
        Err(error) => panic!("native-only failure without interpreter analogue: {error:?}"),
    }
}

fn assert_differential(name: &str, program: &Program<Verified>, limits: &Limits) {
    let expected = interpreter(program, limits);
    let actual = jit(program, limits);
    assert_eq!(actual, expected, "interpreter/JIT mismatch in {name}");
}

fn reg(index: u32) -> Register {
    Register::new(index)
}

fn cid(index: u32) -> ConstantId {
    ConstantId::new(index)
}

/// Emits one observable value through the real `print` host builtin. The
/// computation before this suffix is therefore visible on both execution paths.
fn print_suffix(value: Register, scratch: u32) -> Vec<Instruction> {
    vec![
        Instruction::LoadGlobal {
            dst: reg(scratch),
            name: cid(1),
        },
        Instruction::CreateArray {
            dst: reg(scratch + 1),
        },
        Instruction::ArrayPush {
            array: reg(scratch + 1),
            value,
        },
        Instruction::LoadConst {
            dst: reg(scratch + 2),
            constant: cid(2),
        },
        Instruction::Call {
            dst: reg(scratch + 3),
            callee: reg(scratch),
            this_value: reg(scratch + 2),
            arguments: reg(scratch + 1),
        },
        Instruction::Halt,
    ]
}

fn arithmetic_program(left: i32, right: i32, op: BinaryOp) -> Program<Verified> {
    let mut code = vec![
        Instruction::LoadConst {
            dst: reg(0),
            constant: cid(3),
        },
        Instruction::LoadConst {
            dst: reg(1),
            constant: cid(4),
        },
        Instruction::Binary {
            dst: reg(2),
            op,
            left: reg(0),
            right: reg(1),
        },
    ];
    code.extend(print_suffix(reg(2), 3));
    program(
        vec![
            Constant::String(EcmaString::encode("entry")),
            Constant::String(EcmaString::encode("print")),
            Constant::Undefined,
            Constant::Int32(left),
            Constant::Int32(right),
        ],
        7,
        code,
    )
}

#[derive(Clone, Copy)]
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    fn operand(&mut self) -> i32 {
        (self.next() % 2_001) as i32 - 1_000
    }
}

#[test]
fn every_classified_opcode_and_helper_is_accounted_for() {
    verify_helper_coverage().expect("the landed helper classifier must cover the complete ISA");

    let mut helpers = BTreeSet::new();
    for helper in STRUCTURAL_HELPERS {
        helpers.insert(helper.external_index());
    }
    for instruction in ISA_REPRESENTATIVES {
        match helper_demand(instruction) {
            HelperDemand::Inline => {}
            HelperDemand::Call(helper) => {
                helpers.insert(helper.external_index());
            }
            HelperDemand::Missing { name, contract } => {
                panic!("classified opcode still lacks real helper {name}: {contract}")
            }
        }
    }

    let expected: BTreeSet<_> = (0..bamts_native::HELPER_COUNT).collect();
    assert_eq!(helpers, expected, "every runtime helper must be reachable");
    assert_eq!(ISA_REPRESENTATIVES.len(), 52, "append-only ISA changed");
    assert_eq!(helpers.len(), 47, "runtime helper namespace changed");
}

#[test]
fn deterministic_seeded_arithmetic_matches_interpreter() {
    const SEEDS: [u64; 8] = [
        0,
        1,
        0x5eed,
        0xdead_beef,
        0x0123_4567_89ab_cdef,
        0x8000_0000_0000_0000,
        u64::MAX - 1,
        u64::MAX,
    ];
    const OPS: [BinaryOp; 20] = [
        BinaryOp::Add,
        BinaryOp::Subtract,
        BinaryOp::Multiply,
        BinaryOp::Divide,
        BinaryOp::Remainder,
        BinaryOp::Exponent,
        BinaryOp::BitAnd,
        BinaryOp::BitOr,
        BinaryOp::BitXor,
        BinaryOp::ShiftLeft,
        BinaryOp::ShiftRight,
        BinaryOp::UnsignedShiftRight,
        BinaryOp::Equal,
        BinaryOp::NotEqual,
        BinaryOp::StrictEqual,
        BinaryOp::StrictNotEqual,
        BinaryOp::LessThan,
        BinaryOp::LessThanOrEqual,
        BinaryOp::GreaterThan,
        BinaryOp::GreaterThanOrEqual,
    ];

    for seed in SEEDS {
        let mut random = Lcg(seed);
        for (index, op) in OPS.into_iter().enumerate() {
            let left = random.operand();
            let mut right = random.operand();
            if matches!(op, BinaryOp::Divide | BinaryOp::Remainder) && right == 0 {
                right = 1;
            }
            let case = arithmetic_program(left, right, op);
            assert_differential(
                &format!("seed={seed:#018x}, operator={index}, operands=({left},{right})"),
                &case,
                &Limits::default(),
            );
            // A second compilation proves the corpus is deterministic and that
            // publication does not retain or patch the first executable image.
            assert_differential(
                &format!("repeat seed={seed:#018x}, operator={index}"),
                &case,
                &Limits::default(),
            );
        }
    }
}

#[test]
fn branches_moves_and_normal_completion_match_interpreter() {
    let mut code = vec![
        Instruction::LoadConst {
            dst: reg(0),
            constant: cid(3),
        },
        Instruction::Move {
            dst: reg(1),
            src: reg(0),
        },
        Instruction::JumpIfFalse {
            condition: reg(1),
            target: Pc::new(5),
        },
        Instruction::LoadConst {
            dst: reg(2),
            constant: cid(4),
        },
        Instruction::Jump { target: Pc::new(6) },
        Instruction::LoadConst {
            dst: reg(2),
            constant: cid(5),
        },
    ];
    code.extend(print_suffix(reg(2), 3));
    let case = program(
        vec![
            Constant::String(EcmaString::encode("entry")),
            Constant::String(EcmaString::encode("print")),
            Constant::Undefined,
            Constant::Boolean(true),
            Constant::Int32(11),
            Constant::Int32(99),
        ],
        7,
        code,
    );
    assert_differential("branch/move/normal", &case, &Limits::default());
}

#[test]
fn abrupt_throw_and_fuel_deopt_match_interpreter() {
    let thrown = program(
        vec![
            Constant::String(EcmaString::encode("entry")),
            Constant::Int32(0x2a),
        ],
        1,
        vec![
            Instruction::LoadConst {
                dst: reg(0),
                constant: cid(1),
            },
            Instruction::Throw { value: reg(0) },
        ],
    );
    assert_differential("uncaught throw", &thrown, &Limits::default());

    let spin = program(
        vec![Constant::String(EcmaString::encode("entry"))],
        0,
        vec![Instruction::Jump { target: Pc::new(0) }],
    );
    assert_differential(
        "fuel deopt",
        &spin,
        &Limits {
            fuel: 31,
            ..Limits::default()
        },
    );
}

#[test]
fn osr_deopt_and_cancellation_are_exact_and_terminal() {
    verify_tiering_contract().expect("landed tiering contract must pass");
    let policy = WarmupPolicy {
        baseline_invocations: 3,
        optimized_invocations: 5,
        osr_back_edges: 4,
        deopt_budget: 2,
    };

    let mut warm = TieringState::new(policy).expect("valid stress policy");
    assert_eq!(warm.observe_invocation(), None);
    assert_eq!(warm.observe_invocation(), None);
    assert_eq!(
        warm.observe_invocation(),
        Some(TierTransition {
            from: Tier::Interpreter,
            to: Tier::Baseline,
        })
    );
    assert_eq!(warm.observe_invocation(), None);
    assert_eq!(
        warm.observe_invocation(),
        Some(TierTransition {
            from: Tier::Baseline,
            to: Tier::Optimized,
        })
    );

    let mut osr = TieringState::new(policy).expect("valid stress policy");
    for _ in 0..policy.osr_back_edges - 1 {
        assert_eq!(osr.observe_back_edge(Pc::new(2), Pc::new(9)), None);
    }
    assert_eq!(
        osr.observe_back_edge(Pc::new(2), Pc::new(9)),
        Some(OsrEntry { pc: Pc::new(2) })
    );
    assert_eq!(
        osr.observe_back_edge(Pc::new(9), Pc::new(2)),
        None,
        "forward edges must never become OSR entries"
    );

    assert_eq!(warm.record_deopt(DeoptReason::Panic), Tier::Interpreter);
    assert!(!warm.is_pinned());
    assert_eq!(
        warm.record_deopt(DeoptReason::InvalidRegister),
        Tier::Interpreter
    );
    assert!(warm.is_pinned());
    for _ in 0..policy.optimized_invocations {
        assert_eq!(warm.observe_invocation(), None);
    }

    let mut cancelled = TieringState::new(policy).expect("valid stress policy");
    cancelled.cancel();
    cancelled.cancel();
    assert!(cancelled.is_cancelled());
    assert_eq!(cancelled.observe_invocation(), None);
    assert_eq!(cancelled.observe_back_edge(Pc::new(0), Pc::new(1)), None);
    assert_eq!(
        cancelled.record_deopt(DeoptReason::Panic),
        Tier::Interpreter
    );
    assert!(!cancelled.is_pinned());
}

#[test]
fn executable_publication_is_repeatable_and_program_bound() {
    // `compile_jit` cannot construct `JitProgram` until the private W^X handle
    // has sealed every mapping and minted its FinalizedMemory receipt. Repeated
    // compile/run/drop cycles exercise the public consequence without exposing
    // writable pages or raw entry pointers to this integration test.
    for seed in 0..32 {
        let case = arithmetic_program(seed, seed.wrapping_mul(17), BinaryOp::BitXor);
        let expected = interpreter(&case, &Limits::default());
        for generation in 0..3 {
            let actual = jit(&case, &Limits::default());
            assert_eq!(
                actual, expected,
                "W^X publication changed result at seed {seed}, generation {generation}"
            );
        }
    }
}
