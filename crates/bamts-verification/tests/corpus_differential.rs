use std::{fs, path::Path, process};

use bamts_verification::corpus::{
    BamtsRunner, CaseSpec, CorpusFailure, CorpusStage, ExecutionMode, NodeOracle, OracleOutcome,
    PINNED_CASE_IDS, TASK_106_SYNC_CASE_IDS, TASK_107_NODE_CASE_IDS, load_corpus,
    run_corpus_worker_from_env,
};

#[test]
#[ignore = "spawned by BamtsRunner as a killable JIT/AOT boundary"]
fn corpus_differential_worker() {
    if std::env::var_os("BAMTS_CORPUS_WORKER_REQUEST").is_none() {
        return;
    }
    run_corpus_worker_from_env().expect("corpus worker completes");
}

#[test]
fn every_execution_mode_enforces_the_case_timeout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("corpus-timeout-{}", process::id());
    let entrypoint = format!("target/{id}.ts");
    let path = root.join(&entrypoint);
    fs::write(&path, "while (true) {}\n").expect("write timeout fixture");
    let spec = CaseSpec {
        id,
        repository: "local".to_owned(),
        commit: "0".repeat(40),
        license: "UNLICENSED".to_owned(),
        source_dir: "target".to_owned(),
        entrypoint,
        node_args: Vec::new(),
        expected_timeout_ms: 100,
        constructs: Vec::new(),
        source_files: Vec::new(),
    };
    let bamts = BamtsRunner::new(&root);
    for mode in ExecutionMode::ALL {
        let outcome = bamts
            .run_case(&spec, mode)
            .unwrap_or_else(|error| panic!("{} timeout run failed: {error}", mode.as_str()));
        assert!(
            outcome.timed_out,
            "{} infinite loop escaped its case timeout: {}",
            mode.as_str(),
            evidence(&outcome)
        );
    }
    fs::remove_file(path).expect("remove timeout fixture");
}

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
fn for_await_sync_thenable_unwrap_and_iterator_result_not_assimilated() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("for-await-sync-thenable-{}", process::id());
    let entrypoint = format!("target/{id}.ts");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        r#"(async () => {
  const out: string[] = [];

  for await (const v of [Promise.resolve(42)]) {
    out.push(`promise.unwrapped=${v === 42}`);
  }

  let thenableCounter = 0;
  const thenableIterator = {
    [Symbol.iterator]() {
      let step = 0;
      return {
        next() {
          if (step++ === 0) {
            return {
              value: "raw",
              done: false,
              then(resolve: (value: string) => void, _reject: unknown) {
                thenableCounter += 1;
                resolve("assimilated");
              },
            };
          }
          return { value: undefined, done: true };
        },
      };
    },
  };

  for await (const v of thenableIterator) {
    out.push(`thenable.yield=${v}`);
    out.push(`thenable.counter=${thenableCounter}`);
  }

  let rejectionCloseCount = 0;
  const rejectingIterator = {
    [Symbol.iterator]() {
      let step = 0;
      return {
        next() {
          if (step++ === 0) {
            return { value: Promise.reject("value-rejection"), done: false };
          }
          return { value: undefined, done: true };
        },
        return() {
          rejectionCloseCount = rejectionCloseCount + 1;
          return { value: undefined, done: true };
        },
      };
    },
  };

  try {
    for await (const _v of rejectingIterator) {}
  } catch (error) {
    out.push(`rejection.reason=${error === "value-rejection"}`);
  }
  out.push(`rejection.close-count=${rejectionCloseCount}`);

  let stepFailureCloseCount = 0;
  const failingStepIterator = {
    [Symbol.iterator]() {
      return {
        next() {
          throw "step-failure";
        },
        return() {
          stepFailureCloseCount = stepFailureCloseCount + 1;
          return { value: undefined, done: true };
        },
      };
    },
  };

  try {
    for await (const _v of failingStepIterator) {}
  } catch (error) {
    out.push(`step-failure.reason=${error === "step-failure"}`);
  }
  out.push(`step-failure.close-count=${stepFailureCloseCount}`);
  console.log(out.join("\n"));
})();"#,
    )
    .expect("write for-await thenable fixture");
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
    fs::remove_file(path).expect("remove for-await thenable fixture");
    assert!(
        failures.is_empty(),
        "for-await sync thenable differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn iterator_close_matches_node_in_every_execution_mode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("iterator-close-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(
        &path,
        r#"(async () => {
  const trace = [];
  function sync(tag, values, closeError, closeResult) {
    let index = 0;
    return {
      [Symbol.iterator]() { return this; },
      next() {
        return index < values.length
          ? { value: values[index++], done: false }
          : { value: undefined, done: true };
      },
      return() {
        trace.push(`${tag}:return`);
        if (closeError !== undefined) throw closeError;
        if (closeResult !== undefined) return closeResult;
        return { value: undefined, done: true };
      },
    };
  }

  for (const value of sync("break", [1])) {
    trace.push(`break:body:${value}`);
    break;
  }
  for (const value of sync("normal", [1, 2])) {
    trace.push(`normal:body:${value}`);
  }
  for (const value of sync("continue", [1, 2])) {
    trace.push(`continue:body:${value}`);
    continue;
  }
  for (const value of [1]) {
    trace.push(`absent:body:${value}`);
    break;
  }
  try {
    for (const value of sync("primitive", [1], undefined, 1)) {
      trace.push(`primitive:body:${value}`);
      break;
    }
  } catch (_error) {
    trace.push("primitive:catch");
  }
  function returnFromLoop() {
    for (const value of sync("function", [1])) {
      try {
        trace.push(`function:body:${value}`);
        return "returned";
      } finally {
        trace.push("function:finally");
      }
    }
  }
  trace.push(`function:result:${returnFromLoop()}`);
  try {
    const bindingValue = {
      get value() {
        trace.push("binding:get");
        throw "binding-error";
      },
    };
    for (const { value } of sync("binding", [bindingValue])) {
      trace.push(`binding:body:${value}`);
    }
  } catch (_error) {
    trace.push("binding:catch");
  }
  exit: {
    for (const _value of sync("label-break", [1])) {
      try {
        trace.push("label-break:body");
        break exit;
      } finally {
        trace.push("label-break:finally");
      }
    }
  }
  outer: for (let i = 0; i < 2; i++) {
    for (const _value of sync(`label:${i}`, [i])) {
      try {
        trace.push(`label:body:${i}`);
        continue outer;
      } finally {
        trace.push(`label:finally:${i}`);
      }
    }
  }
  try {
    for (const _value of sync("throw", [1], "close-error")) {
      try {
        trace.push("throw:body");
        throw "body-error";
      } finally {
        trace.push("throw:finally");
      }
    }
  } catch (error) {
    trace.push(`throw:catch:${error}`);
  }
  for await (const value of [1]) {
    trace.push(`async-absent:body:${value}`);
    break;
  }
  const asyncPrimitive = {
    [Symbol.asyncIterator]() {
      return {
        next() { return Promise.resolve({ value: 1, done: false }); },
        return() {
          trace.push("async-primitive:return");
          return Promise.resolve(1);
        },
      };
    },
  };
  try {
    for await (const value of asyncPrimitive) {
      trace.push(`async-primitive:body:${value}`);
      break;
    }
  } catch (_error) {
    trace.push("async-primitive:catch");
  }
  const asyncIterable = {
    [Symbol.asyncIterator]() {
      let started = false;
      return {
        next() {
          if (started) return Promise.resolve({ value: undefined, done: true });
          started = true;
          return Promise.resolve({ value: 1, done: false });
        },
        return() {
          trace.push("async:return");
          return Promise.resolve().then(() => {
            trace.push("async:return-reject");
            throw "async-close-error";
          });
        },
      };
    },
  };
  try {
    for await (const value of asyncIterable) {
      try {
        trace.push(`async:body:${value}`);
        throw "async-body-error";
      } finally {
        trace.push("async:finally");
      }
    }
  } catch (error) {
    trace.push(`async:catch:${error}`);
  }
  console.log(trace.join("\n"));
})();"#,
    )
    .expect("write iterator-close fixture");
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
    fs::remove_file(path).expect("remove iterator-close fixture");
    assert!(
        failures.is_empty(),
        "iterator-close differential failures:\n{}",
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
fn first_tla_rejection_aborts_root_body_before_pending_dependency_completes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("tla-reject-{}", process::id());
    let entrypoint = format!("target/{id}.ts");
    let root_path = root.join(&entrypoint);
    let rejecting_path = root.join(format!("target/{id}-rejecting.ts"));
    let pending_path = root.join(format!("target/{id}-pending.ts"));

    fs::write(
        &rejecting_path,
        "await Promise.resolve();\nthrow 'first-rejection';\n",
    )
    .expect("write rejecting dependency");
    fs::write(
        &pending_path,
        "await Promise.resolve();\nawait Promise.resolve();\n",
    )
    .expect("write pending dependency");
    fs::write(
        &root_path,
        format!(
            "import './{id}-rejecting.ts';\nimport './{id}-pending.ts';\nconsole.log('ROOT-BODY-EXECUTED');\n"
        ),
    )
    .expect("write root fixture");

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
    let oracle = NodeOracle::discover(&root).expect("Node oracle available");
    let expected = oracle.run_case(&spec);
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();
    match &expected {
        Ok(outcome)
            if outcome.is_reliable()
                && outcome.exit_code == Some(1)
                && outcome.stdout.is_empty() => {}
        Ok(outcome) => failures.push(format!(
            "case `{}` / Node: expected a clean rejection with no root output; got {}",
            spec.id,
            evidence(outcome)
        )),
        Err(error) => failures.push(format!(
            "case `{}` / Node: oracle failed before comparison: {error}",
            spec.id
        )),
    }
    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(&spec, mode);
        compare_case(&spec.id, mode, &expected, &actual, &mut failures);
    }
    fs::remove_file(root_path).expect("remove root fixture");
    fs::remove_file(rejecting_path).expect("remove rejecting fixture");
    fs::remove_file(pending_path).expect("remove pending fixture");
    assert!(
        failures.is_empty(),
        "TLA first-rejection differential failures:\n{}",
        failures.join("\n\n")
    );
}

#[test]
fn top_level_throws_are_comparable_process_outcomes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let id = format!("task-113-top-level-throw-{}", process::id());
    let entrypoint = format!("target/{id}.js");
    let path = root.join(&entrypoint);
    fs::write(&path, "throw new Error('boom');\n").expect("write top-level throw fixture");
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
    let oracle = NodeOracle::discover(&root).expect("Node oracle available");
    let expected = oracle.run_case(&spec);
    let bamts = BamtsRunner::new(&root);
    let mut failures = Vec::new();
    for mode in ExecutionMode::ALL {
        let actual = bamts.run_case(&spec, mode);
        compare_case(&spec.id, mode, &expected, &actual, &mut failures);
    }
    fs::remove_file(path).expect("remove top-level throw fixture");
    assert!(
        failures.is_empty(),
        "top-level throw differential failures:\n{}",
        failures.join("\n\n")
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
