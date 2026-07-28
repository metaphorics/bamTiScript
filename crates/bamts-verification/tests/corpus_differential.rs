use std::path::Path;

use bamts_verification::corpus::{
    BamtsRunner, ExecutionMode, NodeOracle, OracleOutcome, PINNED_CASE_IDS, load_corpus,
};

#[test]
fn all_pinned_cases_match_node_in_jit_and_aot() {
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
                "{label}: Node oracle failed: {error}\n{label}: BamTS failed: {actual_error}"
            ));
            return;
        }
        (Err(error), Ok(actual)) => {
            failures.push(format!(
                "{label}: Node oracle failed: {error}\nBamTS evidence: {}",
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
            "{label}: Node oracle did not produce a complete result: {}",
            evidence(expected)
        ));
        return;
    }
    if !actual.is_reliable() {
        failures.push(format!(
            "{label}: BamTS did not produce a complete result: {}",
            evidence(actual)
        ));
        return;
    }
    if expected.exit_code != actual.exit_code || expected.stdout != actual.stdout {
        failures.push(format!(
            "{label}: stdout bytes or exit code differ\nNode: {}\nBamTS: {}\nfirst stdout difference: {}",
            evidence(expected),
            evidence(actual),
            first_difference(&expected.stdout, &actual.stdout),
        ));
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
