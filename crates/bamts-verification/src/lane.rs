//! Lane request/response protocol and parent-only PASS derivation.
//!
//! Workers never mint `PASS`.  They return a completed observation or a closed
//! non-pass outcome.  The parent records `PASS` only when the nonce, obligation
//! key (case, mode, platform), and observable set match exactly.  Timeout,
//! signal, protocol error, and worker crash become blocking receipts, and the
//! lane continues with later cases.  Process execution reuses the corpus
//! normalized environment, bounded pipe drain, process-group timeout, and
//! whole-group termination.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    ErrorCode, Result, VerificationError,
    corpus::{OracleLimits, normalized_env, run_process},
    evidence::{EvidenceRow, TerminalState, WorkingDirectoryPolicy},
    shard::{ObligationKey, require_token},
};

/// Environment variable naming the worker request JSON path.
pub const LANE_WORKER_REQUEST: &str = "BAMTS_LANE_WORKER_REQUEST";

const DEFAULT_MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Parent-issued work item.  The nonce and key are compared exactly on return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneRequest {
    nonce: u64,
    key: ObligationKey,
    observables: BTreeSet<String>,
    argv: Vec<String>,
    working_directory: WorkingDirectoryPolicy,
    timeout_ms: u64,
}

impl LaneRequest {
    pub fn new(
        nonce: u64,
        key: ObligationKey,
        observables: BTreeSet<String>,
        argv: Vec<String>,
        working_directory: WorkingDirectoryPolicy,
        timeout_ms: u64,
    ) -> Result<Self> {
        key.validate()?;
        if observables.is_empty() {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("lane request `{key}` declares no observables"),
            ));
        }
        for observable in &observables {
            require_token("observable", observable)?;
        }
        if timeout_ms == 0 {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                "lane request timeout_ms must be positive",
            ));
        }
        Ok(Self {
            nonce,
            key,
            observables,
            argv,
            working_directory,
            timeout_ms,
        })
    }

    #[must_use]
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    #[must_use]
    pub fn key(&self) -> &ObligationKey {
        &self.key
    }

    #[must_use]
    pub fn observables(&self) -> &BTreeSet<String> {
        &self.observables
    }

    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
}

/// Worker-reported outcome.  There is no `pass` variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaneOutcome {
    Completed { artifacts: BTreeMap<String, String> },
    BlockingFail { detail: String },
    InapplicableLanguageService { detail: String },
    InapplicableOutOfScopeHostFeature { detail: String },
    InapplicableV8Internal { detail: String },
    InapplicableCatalogError { detail: String },
    ExternalBlocked { detail: String },
}

/// Worker response bound to the request nonce and key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneResponse {
    nonce: u64,
    key: ObligationKey,
    outcome: LaneOutcome,
}

impl LaneResponse {
    pub fn new(nonce: u64, key: ObligationKey, outcome: LaneOutcome) -> Result<Self> {
        key.validate()?;
        Ok(Self {
            nonce,
            key,
            outcome,
        })
    }

    #[must_use]
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    #[must_use]
    pub fn key(&self) -> &ObligationKey {
        &self.key
    }

    #[must_use]
    pub fn outcome(&self) -> &LaneOutcome {
        &self.outcome
    }
}

/// What the parent observed about the worker process itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessObservation {
    Exited { code: i32 },
    TimedOut,
    Signaled { number: i32 },
}

/// Raw executor result.  `response_body` is untrusted until the parent parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneProcessResult {
    pub observation: ProcessObservation,
    pub response_body: Option<Vec<u8>>,
    pub duration_ms: u64,
    pub detail: String,
}

/// Executor seam used by the parent lane runner.
pub trait LaneExecutor {
    fn run(&mut self, request: &LaneRequest) -> Result<LaneProcessResult>;
}

/// Spawns a worker process using corpus process mechanics.
#[derive(Debug, Clone)]
pub struct ProcessExecutor {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    work_dir: PathBuf,
    max_output_bytes: usize,
}

impl ProcessExecutor {
    pub fn new(
        program: impl Into<PathBuf>,
        args: Vec<OsString>,
        cwd: impl Into<PathBuf>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: cwd.into(),
            work_dir: work_dir.into(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    pub fn with_max_output_bytes(mut self, cap: usize) -> Self {
        self.max_output_bytes = cap;
        self
    }
}

impl LaneExecutor for ProcessExecutor {
    fn run(&mut self, request: &LaneRequest) -> Result<LaneProcessResult> {
        fs::create_dir_all(&self.work_dir).map_err(|error| {
            VerificationError::new(
                ErrorCode::Io,
                format!("{}: {error}", self.work_dir.display()),
            )
        })?;
        let request_path = self.work_dir.join(format!("lane-{}.json", request.nonce()));
        let response_path = request_path.with_extension("response.json");
        let encoded = serde_json::to_vec(request).map_err(|error| {
            VerificationError::new(ErrorCode::Json, format!("encode lane request: {error}"))
        })?;
        fs::write(&request_path, encoded).map_err(|error| {
            VerificationError::new(
                ErrorCode::Io,
                format!("{}: {error}", request_path.display()),
            )
        })?;
        let _ = fs::remove_file(&response_path);

        let mut environment = normalized_env();
        if let Ok(path) = std::env::var("PATH") {
            environment.push(("PATH".to_owned(), path));
        }
        environment.push((
            LANE_WORKER_REQUEST.to_owned(),
            request_path.to_string_lossy().into_owned(),
        ));
        let limits = OracleLimits {
            timeout: Duration::from_millis(request.timeout_ms()),
            max_output_bytes: self.max_output_bytes,
        };
        let outcome = run_process(
            "lane worker",
            &self.program,
            &self.cwd,
            &environment,
            &self.args,
            &limits,
        )?;
        let response_body = fs::read(&response_path).ok();
        let observation = if outcome.timed_out {
            ProcessObservation::TimedOut
        } else if let Some(number) = outcome.signal {
            ProcessObservation::Signaled { number }
        } else {
            ProcessObservation::Exited {
                code: outcome.exit_code.unwrap_or(1),
            }
        };
        Ok(LaneProcessResult {
            observation,
            response_body,
            duration_ms: request.timeout_ms().min(u64::from(
                u32::try_from(limits.timeout.as_millis()).unwrap_or(u32::MAX),
            )),
            detail: String::from_utf8_lossy(&outcome.stderr).into_owned(),
        })
    }
}

/// Runs every request, recording blocking evidence on failure and continuing.
pub fn run_lane<E: LaneExecutor>(
    executor: &mut E,
    requests: &[LaneRequest],
) -> Result<Vec<EvidenceRow>> {
    let mut rows = Vec::with_capacity(requests.len());
    for request in requests {
        let process = match executor.run(request) {
            Ok(process) => process,
            Err(error) => LaneProcessResult {
                observation: ProcessObservation::Exited { code: 1 },
                response_body: None,
                duration_ms: 0,
                detail: error.to_string(),
            },
        };
        rows.push(derive_row(request, process)?);
    }
    Ok(rows)
}

/// Parent-only derivation: workers cannot mint PASS.
pub fn derive_row(request: &LaneRequest, process: LaneProcessResult) -> Result<EvidenceRow> {
    let (state, artifacts, detail) = match process.observation {
        ProcessObservation::TimedOut => (
            TerminalState::Timeout,
            BTreeMap::new(),
            ignore_pass_attempt(process.response_body.as_deref(), "timeout"),
        ),
        ProcessObservation::Signaled { number } => (
            TerminalState::Signal,
            BTreeMap::new(),
            format!("worker signaled ({number}); {}", process.detail),
        ),
        ProcessObservation::Exited { code } if code != 0 => (
            TerminalState::WorkerCrash,
            BTreeMap::new(),
            format!("worker exited {code}; {}", process.detail),
        ),
        ProcessObservation::Exited { code: _ } => {
            match parse_response(process.response_body.as_deref()) {
                Err(detail) => (TerminalState::ProtocolError, BTreeMap::new(), detail),
                Ok(response) => derive_from_response(request, response),
            }
        }
    };
    EvidenceRow::new(
        request.key().clone(),
        request.argv.clone(),
        request.working_directory,
        request.observables().clone(),
        artifacts,
        state,
        process.duration_ms,
        detail,
    )
}

fn parse_response(body: Option<&[u8]>) -> std::result::Result<LaneResponse, String> {
    let Some(bytes) = body else {
        return Err("worker produced no response".to_owned());
    };
    serde_json::from_slice::<LaneResponse>(bytes)
        .map_err(|error| format!("worker response is not a valid lane response: {error}"))
}

fn derive_from_response(
    request: &LaneRequest,
    response: LaneResponse,
) -> (TerminalState, BTreeMap<String, String>, String) {
    if response.nonce() != request.nonce() {
        return (
            TerminalState::ProtocolError,
            BTreeMap::new(),
            format!(
                "response nonce {} does not match request {}",
                response.nonce(),
                request.nonce()
            ),
        );
    }
    if response.key() != request.key() {
        return (
            TerminalState::ProtocolError,
            BTreeMap::new(),
            format!(
                "response key `{}` does not match request `{}`",
                response.key(),
                request.key()
            ),
        );
    }
    match response.outcome {
        LaneOutcome::Completed { artifacts } => {
            let declared = request.observables();
            let observed: BTreeSet<String> = artifacts.keys().cloned().collect();
            if &observed != declared {
                return (
                    TerminalState::ProtocolError,
                    BTreeMap::new(),
                    format!(
                        "observable set mismatch for `{}`: declared {declared:?}, observed {observed:?}",
                        request.key()
                    ),
                );
            }
            (TerminalState::Pass, artifacts, String::new())
        }
        LaneOutcome::BlockingFail { detail } => {
            (TerminalState::BlockingFail, BTreeMap::new(), detail)
        }
        LaneOutcome::InapplicableLanguageService { detail } => (
            TerminalState::InapplicableLanguageService,
            BTreeMap::new(),
            detail,
        ),
        LaneOutcome::InapplicableOutOfScopeHostFeature { detail } => (
            TerminalState::InapplicableOutOfScopeHostFeature,
            BTreeMap::new(),
            detail,
        ),
        LaneOutcome::InapplicableV8Internal { detail } => (
            TerminalState::InapplicableV8Internal,
            BTreeMap::new(),
            detail,
        ),
        LaneOutcome::InapplicableCatalogError { detail } => (
            TerminalState::InapplicableCatalogError,
            BTreeMap::new(),
            detail,
        ),
        LaneOutcome::ExternalBlocked { detail } => {
            (TerminalState::ExternalBlocked, BTreeMap::new(), detail)
        }
    }
}

fn ignore_pass_attempt(body: Option<&[u8]>, cause: &str) -> String {
    match parse_response(body) {
        Ok(response) if matches!(response.outcome(), LaneOutcome::Completed { .. }) => {
            format!("{cause}: ignoring completed/PASS attempt from a failed worker")
        }
        _ => cause.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::ExecutionMode;
    use std::collections::BTreeMap;

    struct ScriptedExecutor {
        results: Vec<LaneProcessResult>,
        index: usize,
    }

    impl ScriptedExecutor {
        fn new(results: Vec<LaneProcessResult>) -> Self {
            Self { results, index: 0 }
        }
    }

    impl LaneExecutor for ScriptedExecutor {
        fn run(&mut self, _request: &LaneRequest) -> Result<LaneProcessResult> {
            let result = self.results.get(self.index).cloned().ok_or_else(|| {
                VerificationError::new(ErrorCode::Schema, "scripted executor exhausted")
            })?;
            self.index += 1;
            Ok(result)
        }
    }

    fn key(mode: ExecutionMode, platform: &str, case: &str) -> ObligationKey {
        ObligationKey::new("typescript-7.0.2", case, "default", mode, platform).expect("key")
    }

    fn request(nonce: u64, key: ObligationKey) -> LaneRequest {
        LaneRequest::new(
            nonce,
            key,
            BTreeSet::from(["stdout".to_owned()]),
            vec!["lane".to_owned()],
            WorkingDirectoryPolicy::RepositoryRoot,
            1_000,
        )
        .expect("request")
    }

    fn completed_body(
        nonce: u64,
        key: &ObligationKey,
        artifacts: BTreeMap<String, String>,
    ) -> Vec<u8> {
        let response = LaneResponse::new(nonce, key.clone(), LaneOutcome::Completed { artifacts })
            .expect("response");
        serde_json::to_vec(&response).expect("encode")
    }

    fn stdout_ok() -> BTreeMap<String, String> {
        BTreeMap::from([(
            "stdout".to_owned(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        )])
    }

    fn success(nonce: u64, key: &ObligationKey) -> LaneProcessResult {
        LaneProcessResult {
            observation: ProcessObservation::Exited { code: 0 },
            response_body: Some(completed_body(nonce, key, stdout_ok())),
            duration_ms: 3,
            detail: String::new(),
        }
    }

    #[test]
    fn parent_derives_pass_from_exact_response() {
        let good_key = key(
            ExecutionMode::Interpreter,
            "x86_64-unknown-linux-gnu",
            "case-0001",
        );
        let request = request(7, good_key.clone());
        let row = derive_row(&request, success(7, &good_key)).expect("derive");
        assert_eq!(row.state(), TerminalState::Pass);
        assert_eq!(row.key(), &good_key);
        assert_eq!(row.artifacts().len(), 1);

        let mismatches = [
            ("nonce", success(8, &good_key)),
            (
                "case",
                success(
                    7,
                    &key(
                        ExecutionMode::Interpreter,
                        "x86_64-unknown-linux-gnu",
                        "case-9999",
                    ),
                ),
            ),
            (
                "mode",
                success(
                    7,
                    &key(ExecutionMode::Jit, "x86_64-unknown-linux-gnu", "case-0001"),
                ),
            ),
            (
                "platform",
                success(
                    7,
                    &key(
                        ExecutionMode::Interpreter,
                        "aarch64-unknown-linux-gnu",
                        "case-0001",
                    ),
                ),
            ),
        ];
        for (name, process) in mismatches {
            let row = derive_row(&request, process).expect(name);
            assert_eq!(row.state(), TerminalState::ProtocolError, "{name}");
            assert_ne!(row.state(), TerminalState::Pass, "{name}");
        }

        let mut extras = stdout_ok();
        extras.insert(
            "stderr".to_owned(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
        );
        let wrong_obs = LaneProcessResult {
            observation: ProcessObservation::Exited { code: 0 },
            response_body: Some(completed_body(7, &good_key, extras)),
            duration_ms: 3,
            detail: String::new(),
        };
        let row = derive_row(&request, wrong_obs).expect("observables");
        assert_eq!(row.state(), TerminalState::ProtocolError);
    }

    #[test]
    fn records_failure_and_continues() {
        let keys: Vec<ObligationKey> = (1..=5)
            .map(|index| {
                key(
                    ExecutionMode::Interpreter,
                    "x86_64-unknown-linux-gnu",
                    &format!("case-{index:04}"),
                )
            })
            .collect();
        let requests: Vec<LaneRequest> = keys
            .iter()
            .enumerate()
            .map(|(index, item)| request((index as u64) + 1, item.clone()))
            .collect();
        let timeout = LaneProcessResult {
            observation: ProcessObservation::TimedOut,
            response_body: Some(completed_body(1, &keys[0], stdout_ok())),
            duration_ms: 200,
            detail: "timeout".to_owned(),
        };
        let signaled = LaneProcessResult {
            observation: ProcessObservation::Signaled { number: 9 },
            response_body: None,
            duration_ms: 1,
            detail: "killed".to_owned(),
        };
        let protocol = LaneProcessResult {
            observation: ProcessObservation::Exited { code: 0 },
            response_body: None,
            duration_ms: 1,
            detail: String::new(),
        };
        let crash = LaneProcessResult {
            observation: ProcessObservation::Exited { code: 1 },
            response_body: None,
            duration_ms: 1,
            detail: "boom".to_owned(),
        };
        let mut executor = ScriptedExecutor::new(vec![
            timeout,
            signaled,
            protocol,
            crash,
            success(5, &keys[4]),
        ]);
        let rows = run_lane(&mut executor, &requests).expect("lane");
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].state(), TerminalState::Timeout);
        assert_eq!(rows[1].state(), TerminalState::Signal);
        assert_eq!(rows[2].state(), TerminalState::ProtocolError);
        assert_eq!(rows[3].state(), TerminalState::WorkerCrash);
        assert_eq!(rows[4].state(), TerminalState::Pass);
        assert!(
            rows.iter()
                .take(4)
                .all(|row| row.state() != TerminalState::Pass)
        );
    }

    #[test]
    fn process_executor_reuses_corpus_timeout() {
        let scratch = std::env::temp_dir().join(format!(
            "bamts-lane-timeout-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&scratch).expect("scratch");
        let mut executor =
            ProcessExecutor::new("/bin/sleep", vec![OsString::from("2")], "/", &scratch)
                .with_max_output_bytes(64);
        let item = key(
            ExecutionMode::Interpreter,
            "x86_64-unknown-linux-gnu",
            "case-sleep",
        );
        let request = LaneRequest::new(
            1,
            item,
            BTreeSet::from(["stdout".to_owned()]),
            vec!["sleep".to_owned()],
            WorkingDirectoryPolicy::RepositoryRoot,
            200,
        )
        .expect("short timeout");
        let result = executor.run(&request).expect("run");
        let _ = fs::remove_dir_all(&scratch);
        assert!(matches!(result.observation, ProcessObservation::TimedOut));
    }
}
