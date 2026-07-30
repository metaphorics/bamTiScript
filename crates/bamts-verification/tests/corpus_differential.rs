use std::{fs, path::Path, process};

use bamts_verification::corpus::{
    BamtsRunner, CaseSpec, CorpusFailure, CorpusStage, ExecutionMode, NodeOracle, OracleOutcome,
    PINNED_CASE_IDS, TASK_106_SYNC_CASE_IDS, TASK_107_NODE_CASE_IDS, load_corpus,
};

#[test]
fn all_pinned_cases_match_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let corpus = load_corpus(&root).expect("the pinned corpus must parse and validate");
    let ids = corpus
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids, PINNED_CASE_IDS,
        "the differential harness must cover exactly the 20 pinned cases in manifest order"
    );

    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();

    for case in &corpus.cases {
        let expected = oracle.run_case(case);
        for mode in ExecutionMode::ALL {
            let actual = bamts.run_case(case, mode);
            compare_case(&case.id, mode, &expected, &actual, &mut failures);
        }
    }

    assert!(
        failures.is_empty(),
        "corpus differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn p_map_matches_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let corpus = load_corpus(&root).expect("the pinned corpus must parse and validate");
    let case = corpus.case("p-map").expect("p-map is pinned");
    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let expected = oracle.run_case(case);
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();

    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(case, mode);
        compare_case(&case.id, mode, &expected, &actual, &mut failures);
    }

    assert!(
        failures.is_empty(),
        "p-map differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn declaration_owned_self_captures_match_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-106-self-captures-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        "function run() {
  const arrow = () => arrow;
  function recursive() { return recursive; }
  class Self { method() { return Self; } }
  const { destructured = () => destructured } = {};
  const shadowed = () => shadowed;
  { const shadowed = 0; }
  console.log(arrow() === arrow);
  console.log(recursive() === recursive);
  console.log(new Self().method() === Self);
  console.log(destructured() === destructured);
  console.log(shadowed() === shadowed);
}
run();
",
    )
    .expect("write self-capture fixture");
    let spec = CaseSpec {
        id,
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint,
        node_args: Vec::new(),
        expected_timeout_ms: 10_000,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let expected = oracle.run_case(&spec);
    let mut failures = Vec::new();
    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(&spec, mode);
        compare_case(&spec.id, mode, &expected, &actual, &mut failures);
    }
    fs::remove_file(path).expect("remove self-capture fixture");
    assert!(
        failures.is_empty(),
        "declaration-owned self-capture differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn local_lexical_tdz_matches_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-106-local-tdz-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        "function outcome(operation) {
  try { return String(operation()); }
  catch (error) { return error.name; }
}
function earlyRead() {
  const read = () => later;
  const observed = outcome(read);
  let later = 1;
  return observed;
}
function lateRead() {
  const read = () => later;
  let later = 1;
  return read();
}
function hoisted() {
  return declaredLater();
  function declaredLater() { return 2; }
}
function blockRead() {
  let value = 1;
  {
    const read = () => value;
    const observed = outcome(read);
    let value = 2;
    return observed;
  }
}
function localClassName() {
  class LocalClass { static value = LocalClass; }
  return LocalClass.value === LocalClass;
}
const C = class {};
const heritage = outcome(() => { class C extends C {}; return C; });
const expressionHeritage = outcome(() => class C extends C {});
class StaticClass { static value = StaticClass; }
const staticClassName = String(StaticClass.value === StaticClass);
const typeResult = outcome(() => { return typeof value; let value; });
process.stdout.write([
  earlyRead(),
  lateRead(),
  hoisted(),
  heritage,
  expressionHeritage,
  staticClassName,
  String(localClassName()),
  blockRead(),
  typeResult,
].join('\\n') + '\\n');
",
    )
    .expect("write local TDZ fixture");
    let spec = CaseSpec {
        id,
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint,
        node_args: Vec::new(),
        expected_timeout_ms: 10_000,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let expected = oracle.run_case(&spec);
    let mut failures = Vec::new();
    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(&spec, mode);
        compare_case(&spec.id, mode, &expected, &actual, &mut failures);
    }
    fs::remove_file(path).expect("remove local TDZ fixture");
    assert!(
        failures.is_empty(),
        "local lexical TDZ differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn task_106_sync_cases_match_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let corpus = load_corpus(&root).expect("the pinned corpus must parse and validate");
    let ids = corpus
        .cases
        .iter()
        .filter(|case| TASK_106_SYNC_CASE_IDS.contains(&case.id.as_str()))
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids, TASK_106_SYNC_CASE_IDS,
        "the Task 106 gate must cover exactly its synchronous cases in manifest order"
    );

    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();
    for id in TASK_106_SYNC_CASE_IDS {
        let case = corpus.case(id).expect("Task 106 case must exist");
        let expected = oracle.run_case(case);
        for mode in ExecutionMode::ALL {
            let actual = bamts.run_case(case, mode);
            compare_case(&case.id, mode, &expected, &actual, &mut failures);
        }
    }
    assert!(
        failures.is_empty(),
        "Task 106 differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn task_107_node_cases_match_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let corpus = load_corpus(&root).expect("the pinned corpus must parse and validate");
    let ids = corpus
        .cases
        .iter()
        .filter(|case| TASK_107_NODE_CASE_IDS.contains(&case.id.as_str()))
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids, TASK_107_NODE_CASE_IDS,
        "the Task 107 gate must cover exactly citty, is-plain-obj, and ohash in manifest order"
    );

    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();
    for id in TASK_107_NODE_CASE_IDS {
        let case = corpus.case(id).expect("Task 107 case must exist");
        let expected = oracle.run_case(case);
        for mode in ExecutionMode::ALL {
            let actual = bamts.run_case(case, mode);
            compare_case(&case.id, mode, &expected, &actual, &mut failures);
        }
    }
    assert!(
        failures.is_empty(),
        "Task 107 differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn task_110_event_loop_drives_to_quiescence_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-110-event-loop-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        "console.log('sync');
Promise.resolve().then(() => console.log('micro'));
setTimeout(() => {
  console.log('timer');
  Promise.resolve().then(() => console.log('timer-micro'));
}, 1);
",
    )
    .expect("write event-loop fixture");
    let spec = CaseSpec {
        id,
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint,
        node_args: Vec::new(),
        expected_timeout_ms: 10_000,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let oracle = NodeOracle::discover(&root).expect("the pinned Node oracle must be available");
    let bamts = BamtsRunner::new(&root);
    let expected = oracle.run_case(&spec);
    let mut failures = Vec::new();
    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(&spec, mode);
        compare_case(&spec.id, mode, &expected, &actual, &mut failures);
    }
    fs::remove_file(path).expect("remove event-loop fixture");
    assert!(
        failures.is_empty(),
        "Task 110 event-loop differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn interpreter_runtime_errors_classify_as_evaluate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-113-interpreter-throw-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        "throw new Error('boom');
",
    )
    .expect("write interpreter throw fixture");
    let spec = CaseSpec {
        id,
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint,
        node_args: Vec::new(),
        expected_timeout_ms: 10_000,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let bamts = BamtsRunner::new(&root);
    let result = bamts.run_case(&spec, ExecutionMode::Interpreter);
    fs::remove_file(path).expect("remove interpreter throw fixture");
    let error = result.expect_err("a top-level throw must fail in interpreter mode");
    let text = error.to_string();
    assert!(
        text.contains("failed in interpreter mode"),
        "failure must name interpreter mode: {text}"
    );
    assert!(
        text.contains("stage=evaluate"),
        "a runtime throw must classify as evaluate: {text}"
    );
}

#[test]
fn interpreter_failure_text_names_mode_stage_and_evidence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-113-interpreter-missing-{}", process::id());
    let spec = CaseSpec {
        id: id.clone(),
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint: format!("target/{id}.js"),
        node_args: Vec::new(),
        expected_timeout_ms: 10_000,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let bamts = BamtsRunner::new(&root);
    let error = bamts
        .run_case(&spec, ExecutionMode::Interpreter)
        .expect_err("a missing entrypoint must fail in interpreter mode");
    let text = error.to_string();
    assert!(
        text.contains(&format!("case `{id}` failed in interpreter mode")),
        "failure must name the case and interpreter mode: {text}"
    );
    assert!(
        text.contains("stage=load"),
        "a missing entrypoint must classify as load: {text}"
    );
    assert!(
        text.contains("cannot read"),
        "failure must carry the observed evidence: {text}"
    );
}

fn compare_case(
    case_id: &str,
    mode: ExecutionMode,
    expected: &bamts_verification::Result<OracleOutcome>,
    actual: &bamts_verification::Result<OracleOutcome>,
    failures: &mut Vec<String>,
) {
    let label = format!("case `{case_id}` / {}", mode.as_str());
    let (expected, actual) = match (expected, actual) {
        (Err(error), Err(actual_error)) => {
            failures.push(format!(
                "{label}: stage=spawn Node oracle failed: {error}\n{label}: BamTS failed: {actual_error}"
            ));
            return;
        }
        (Err(error), Ok(actual)) => {
            failures.push(format!(
                "{label}: stage=spawn Node oracle failed: {error}\nBamTS evidence: {}",
                evidence(actual)
            ));
            return;
        }
        (Ok(expected), Err(error)) => {
            failures.push(format!(
                "{label}: BamTS failed: {error}\nNode evidence: {}",
                evidence(expected)
            ));
            return;
        }
        (Ok(expected), Ok(actual)) => (expected, actual),
    };

    if !expected.is_reliable() {
        failures.push(format!(
            "{label}: stage=spawn Node oracle did not produce a complete result: {}",
            evidence(expected)
        ));
        return;
    }
    if !actual.is_reliable() {
        failures.push(format!(
            "{label}: stage={} BamTS did not produce a complete result: {}",
            bamts_outcome_stage(mode).as_str(),
            evidence(actual)
        ));
        return;
    }
    if expected.exit_code != actual.exit_code || expected.stdout != actual.stdout {
        failures.push(format!(
            "{label}: stage=compare stdout bytes or exit code differ\nNode: {}\nBamTS: {}\nfirst stdout difference: {}",
            evidence(expected),
            evidence(actual),
            first_difference(&expected.stdout, &actual.stdout),
        ));
    }
}

fn bamts_outcome_stage(mode: ExecutionMode) -> CorpusStage {
    match mode {
        ExecutionMode::Interpreter | ExecutionMode::Jit => CorpusStage::Evaluate,
        ExecutionMode::Aot => CorpusStage::Spawn,
    }
}

fn evidence(outcome: &OracleOutcome) -> String {
    format!(
        "exit={:?}, signal={:?}, timed_out={}, stdout_len={}, stdout_truncated={}, stdout=`{}`, stderr_len={}, stderr_truncated={}, stderr=`{}`",
        outcome.exit_code,
        outcome.signal,
        outcome.timed_out,
        outcome.stdout.len(),
        outcome.stdout_truncated,
        preview(&outcome.stdout),
        outcome.stderr.len(),
        outcome.stderr_truncated,
        preview(&outcome.stderr),
    )
}

fn first_difference(expected: &[u8], actual: &[u8]) -> String {
    let shared = expected.len().min(actual.len());
    if let Some(index) = (0..shared).find(|&index| expected[index] != actual[index]) {
        return format!(
            "byte {index}: Node=0x{:02x}, BamTS=0x{:02x}",
            expected[index], actual[index]
        );
    }
    format!(
        "shared prefix is identical; Node length={}, BamTS length={}",
        expected.len(),
        actual.len()
    )
}

fn preview(bytes: &[u8]) -> String {
    const PREVIEW_BYTES: usize = 256;
    let mut rendered = bytes
        .iter()
        .take(PREVIEW_BYTES)
        .flat_map(|byte| byte.escape_ascii())
        .map(char::from)
        .collect::<String>();
    if bytes.len() > PREVIEW_BYTES {
        rendered.push_str("...");
    }
    rendered
}

#[cfg(test)]
mod formatting_tests {
    use std::ffi::OsString;

    use bamts_bytecode::{ConstantId, FunctionId, Instruction, Pc, Register};
    use bamts_cli::driver::DriverError;
    use bamts_runtime::{NativeError, RuntimeError, RuntimeErrorKind, RuntimeSource};

    use super::*;

    fn outcome(stdout: &[u8]) -> OracleOutcome {
        OracleOutcome {
            timed_out: false,
            exit_code: Some(0),
            signal: None,
            stdout: stdout.to_vec(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        }
    }

    #[test]
    fn formats_check_failure_with_its_first_diagnostic_code() {
        let failure = CorpusFailure::from_driver_error(&DriverError::Diagnostics {
            rendered: "error BAMTS-C004: unresolved name".to_owned(),
        });

        assert_eq!(failure.stage, CorpusStage::Check);
        assert!(
            failure
                .to_string()
                .contains("stage=check: diagnostic=BAMTS-C004")
        );
    }

    #[test]
    fn formats_import_at_pc_zero_as_runtime_evaluation() {
        let failure = CorpusFailure::from_driver_error(&DriverError::Native(NativeError::Runtime(
            RuntimeError {
                kind: RuntimeErrorKind::FuelExhausted { limit: 1 },
                function: FunctionId::new(0),
                pc: Pc::new(0),
                source: RuntimeSource {
                    function_name: None,
                    instruction: Instruction::Import {
                        dst: Register::new(0),
                        specifier: ConstantId::new(0),
                    },
                },
            },
        )));

        let rendered = failure.to_string();
        assert_eq!(failure.stage, CorpusStage::Evaluate);
        assert!(rendered.contains("runtime function=0 pc=0 opcode=Import"));
    }

    #[test]
    fn formats_link_failure_at_link_stage() {
        let failure = CorpusFailure::from_driver_error(&DriverError::ToolchainMissing {
            program: OsString::from("missing-cc"),
        });

        assert_eq!(failure.stage, CorpusStage::Link);
        assert!(failure.to_string().starts_with("stage=link:"));
    }

    #[test]
    fn formats_timeout_at_the_execution_stage() {
        let expected = OracleOutcome {
            timed_out: true,
            exit_code: None,
            signal: Some(9),
            stdout: b"partial".to_vec(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        };
        let actual = outcome(b"partial");
        let mut failures = Vec::new();

        compare_case(
            "timeout",
            ExecutionMode::Jit,
            &Ok(expected),
            &Ok(actual),
            &mut failures,
        );

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("stage=spawn Node oracle did not produce a complete result"));
        assert!(failures[0].contains("stdout=`partial`"));
    }

    #[test]
    fn formats_byte_difference_at_compare_stage() {
        let mut failures = Vec::new();

        compare_case(
            "bytes",
            ExecutionMode::Aot,
            &Ok(outcome(b"a")),
            &Ok(outcome(b"b")),
            &mut failures,
        );

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("stage=compare stdout bytes or exit code differ"));
        assert!(failures[0].contains("byte 0: Node=0x61, BamTS=0x62"));
    }
}
