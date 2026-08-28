//! Suite run/merge engine behind `suite run` and `suite merge`.
//!
//! A run loads the current locked manifest, converts every logical
//! identifier of the selected catalog into an exact obligation key plus its
//! declared observable, strides the canonical catalog by the requested
//! shard, executes each selected obligation through the real repository
//! path behind [`LaneExecutor`], and streams one strict receipt.  Only the
//! parent lane ([`derive_row`]) can record `PASS`; this module never
//! decides or reclassifies a case.  Blocking per-case outcomes are recorded
//! and the loop continues.
//!
//! A merge discovers JSONL shard documents below a receipts root, rejects
//! wrong catalog/runner/platform, absent/duplicate/stale rows, mixed run
//! bindings, and foreign shard matrices, then delegates the bounded k-way
//! merge to [`crate::evidence::merge_shards`].
//!
//! Runner bindings between the parsed key mode and the workflow-declared
//! runner follow the matrix the four workflows declare:
//! TypeScript→compiler, test262→interpreter|jit|aot, formal-quint→quint,
//! formal-lean→lean, formal-redex→redex, target-cells→aot, and
//! benchmarks→perf.  Runners that do not name a runtime mode bind
//! [`ExecutionMode::Aot`]: their obligations execute the AOT candidate
//! substrate, so every (catalog, mode) pair recovers exactly one runner.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufWriter, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    ErrorCode, Result, VerificationError,
    catalog::{self, CatalogCell},
    classification::{self, ClassificationState, NonPassState},
    evidence::{
        EvidenceHeader, EvidenceReader, EvidenceRow, EvidenceWriter, PublishMode, RunBinding,
        TerminalState, WorkingDirectoryPolicy, merge_shards,
    },
    lane::{
        LaneBinding, LaneExecutor, LaneOutcome, LaneProcessResult, LaneRequest, LaneResponse,
        ProcessExecutor, ProcessObservation, derive_row,
    },
    oracles::{self, ProcessBoundary},
    schema,
    shard::{ObligationKey, ShardIdentity, ShardSpec, validate_catalog},
    suite::{DEFAULT_SNAPSHOT_REL, verify_snapshot},
    toolchain_schema::load_target_cells,
};

/// Per-obligation wall-clock bound handed to the lane.
const CASE_TIMEOUT_MS: u64 = 30_000;
/// Manifest schema tag produced by `catalog regenerate`.
const MANIFEST_SCHEMA: &str = "bamti.verification-manifest/v1";
/// Bounded runtime for host-identity probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_OUTPUT_BYTES: usize = 1 << 16;

/// CLIs and workers take this API verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteRunRequest {
    pub catalog: String,
    pub shard: ShardSpec,
    pub receipt: PathBuf,
    pub runner: String,
    pub platform: String,
}

/// Inputs for a deterministic receipt merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteMergeRequest {
    pub catalog: String,
    pub receipts: PathBuf,
    pub out: PathBuf,
    pub publish: PublishMode,
}

/// Counted outcome of a run or a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteReport {
    pub catalog: String,
    pub runner: String,
    pub platform: String,
    pub obligations: usize,
    pub rows: usize,
    pub states: BTreeMap<String, usize>,
    pub obligation_set_digest: String,
    pub out: PathBuf,
    /// Run shard, or the unsharded identity a merge published.
    pub shard: ShardSpec,
    /// Shard documents consumed (0 for a run).
    pub documents: usize,
}

/// Closed runner set declared by the workflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuiteRunner {
    Compiler,
    Interpreter,
    Jit,
    Aot,
    Quint,
    Lean,
    Redex,
    Perf,
}

impl SuiteRunner {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compiler => "compiler",
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
            Self::Aot => "aot",
            Self::Quint => "quint",
            Self::Lean => "lean",
            Self::Redex => "redex",
            Self::Perf => "perf",
        }
    }

    /// Key mode this runner binds.  Non-runtime runners execute the AOT
    /// candidate substrate and therefore bind `ExecutionMode::Aot`.
    #[must_use]
    pub const fn execution_mode(self) -> crate::shard::ExecutionMode {
        match self {
            Self::Interpreter => crate::shard::ExecutionMode::Interpreter,
            Self::Jit => crate::shard::ExecutionMode::Jit,
            Self::Compiler | Self::Aot | Self::Quint | Self::Lean | Self::Redex | Self::Perf => {
                crate::shard::ExecutionMode::Aot
            }
        }
    }
}

/// Workflow-declared runner allowlist for one catalog.
fn allowed_runners(catalog: &str) -> Result<&'static [SuiteRunner]> {
    if catalog.starts_with("typescript-") {
        return Ok(&[SuiteRunner::Compiler]);
    }
    match catalog {
        "test262" => Ok(&[SuiteRunner::Interpreter, SuiteRunner::Jit, SuiteRunner::Aot]),
        "formal-quint" => Ok(&[SuiteRunner::Quint]),
        "formal-lean" => Ok(&[SuiteRunner::Lean]),
        "formal-redex" => Ok(&[SuiteRunner::Redex]),
        "target-cells" => Ok(&[SuiteRunner::Aot]),
        "benchmarks" => Ok(&[SuiteRunner::Perf]),
        _ => Err(VerificationError::new(
            ErrorCode::Usage,
            format!("unknown catalog `{catalog}` for suite selection"),
        )),
    }
}

/// Resolves the requested runner against the workflow-declared mapping.
///
/// An empty runner selects the catalog's sole declared runner; catalogs
/// with several declared runners require an explicit `BAMTS_MODE`.
pub fn resolve_runner(catalog: &str, runner: &str) -> Result<SuiteRunner> {
    // Reject names outside the locked manifest's catalogue before aliases.
    if !schema::CATALOG_NAMES.contains(&catalog) {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            format!("unknown catalog `{catalog}`"),
        ));
    }
    let allowed = allowed_runners(catalog)?;
    if runner.is_empty() {
        return match allowed {
            [sole] => Ok(*sole),
            _ => Err(VerificationError::new(
                ErrorCode::Usage,
                format!(
                    "catalog `{catalog}` declares runners [{}]; `BAMTS_MODE` must choose one",
                    allowed
                        .iter()
                        .map(|entry| entry.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        };
    }
    allowed
        .iter()
        .find(|entry| entry.as_str() == runner)
        .copied()
        .ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "runner `{runner}` is not workflow-declared for catalog `{catalog}` (allowed: [{}])",
                    allowed
                        .iter()
                        .map(|entry| entry.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
}

/// Recovers the runner a receipt row's key mode binds for `catalog`.
///
/// Returns `None` when the pair is not workflow-declared; merges reject it.
fn runner_for_mode(catalog: &str, mode: crate::shard::ExecutionMode) -> Option<SuiteRunner> {
    let allowed = allowed_runners(catalog).ok()?;
    use crate::shard::ExecutionMode as Mode;
    let runner = match mode {
        Mode::Interpreter => SuiteRunner::Interpreter,
        Mode::Jit => SuiteRunner::Jit,
        Mode::Aot => {
            if catalog.starts_with("typescript-") {
                SuiteRunner::Compiler
            } else {
                match catalog {
                    "test262" => SuiteRunner::Aot,
                    "formal-quint" => SuiteRunner::Quint,
                    "formal-lean" => SuiteRunner::Lean,
                    "formal-redex" => SuiteRunner::Redex,
                    "target-cells" => SuiteRunner::Aot,
                    "benchmarks" => SuiteRunner::Perf,
                    _ => return None,
                }
            }
        }
    };
    allowed.contains(&runner).then_some(runner)
}

/// Platform token used when `BAMTS_PLATFORM` is unset: the host triple in
/// the form the receipts already record.
#[must_use]
pub fn default_platform() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        os => format!("{arch}-unknown-{os}"),
    }
}

/// Selected catalog plus the digests that name the current manifest image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogManifest {
    pub catalog: String,
    pub identifiers: Vec<String>,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub source_ledger_sha256: String,
    pub identifiers_sha256: String,
}

/// Loads the locked manifest and proves the selected catalog's identity:
/// schema tag, sources-ledger pin, identifier count, digest, order.
pub fn load_catalog_manifest(root: &Path, catalog: &str) -> Result<CatalogManifest> {
    if !schema::CATALOG_NAMES.contains(&catalog) {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            format!("unknown catalog `{catalog}`"),
        ));
    }
    let manifest_path = root.join(schema::MANIFEST_PATH);
    let bytes = schema::read_bytes(&manifest_path)?;
    let manifest_sha256 = schema::sha256_hex(&bytes);
    schema::reject_duplicate_json_keys(&manifest_path, &bytes)?;
    let manifest: schema::VerificationManifest = schema::parse_json(&manifest_path, &bytes)?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(schema::schema_error(
            &manifest_path,
            format!(
                "expected schema `{MANIFEST_SCHEMA}`, found `{}`",
                manifest.schema
            ),
        ));
    }
    let sources_bytes = schema::read_bytes(&root.join(schema::SOURCES_PATH))?;
    let source_ledger_sha256 = schema::sha256_hex(&sources_bytes);
    if manifest.source_ledger_sha256 != source_ledger_sha256 {
        return Err(schema::schema_error(
            &manifest_path,
            "manifest source ledger digest does not match the current sources pin",
        ));
    }
    let selected = manifest
        .catalogs
        .iter()
        .find(|entry| entry.id == catalog)
        .ok_or_else(|| {
            schema::schema_error(
                &manifest_path,
                format!("manifest has no catalog `{catalog}`"),
            )
        })?;
    if selected.identifier_count != selected.identifiers.len() {
        return Err(schema::schema_error(
            &manifest_path,
            format!(
                "catalog `{catalog}` declares {} identifiers but carries {}",
                selected.identifier_count,
                selected.identifiers.len()
            ),
        ));
    }
    let identifiers_sha256 = schema::identifiers_sha256(&selected.identifiers);
    if selected.identifiers_sha256 != identifiers_sha256 {
        return Err(schema::schema_error(
            &manifest_path,
            format!("catalog `{catalog}` identifier digest does not match its list"),
        ));
    }
    for pair in selected.identifiers.windows(2) {
        if pair[0] >= pair[1] {
            return Err(schema::schema_error(
                &manifest_path,
                format!("catalog `{catalog}` identifiers are not strictly increasing"),
            ));
        }
    }
    Ok(CatalogManifest {
        catalog: catalog.to_owned(),
        identifiers: selected.identifiers.clone(),
        manifest_path,
        manifest_sha256,
        source_ledger_sha256,
        identifiers_sha256,
    })
}

/// One exact obligation: canonical lane key plus its declared observables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalObligation {
    pub key: ObligationKey,
    pub observables: BTreeSet<String>,
}

/// Converts every logical identifier of the catalog into an exact
/// obligation key and declared observable set, in canonical sorted order.
///
/// TypeScript/test262 identifiers are `authority/runner/case#config#observable`;
/// formal and target identifiers are `locator::name`; benchmark identifiers
/// are the plain rule name.  Keys are built injectively: the observable is
/// folded into the configuration token so one case can carry several
/// observables without colliding.
pub fn materialize_obligations(
    catalog: &str,
    identifiers: &[String],
    runner: SuiteRunner,
    platform: &str,
) -> Result<Vec<LogicalObligation>> {
    if platform.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "obligation platform must be nonempty",
        ));
    }
    let mode = runner.execution_mode();
    let mut obligations = Vec::with_capacity(identifiers.len());
    let authority_prefix = format!("{catalog}/");
    for identifier in identifiers {
        let (case, configuration, observables) = parse_logical_identifier(catalog, identifier)?;
        // TS/test262 cases are `runner/path` tokens after the authority
        // prefix; every other family keeps its own identity segments.
        let key = ObligationKey::new(
            catalog,
            case.strip_prefix(authority_prefix.as_str())
                .map(str::to_owned)
                .unwrap_or(case.clone()),
            configuration,
            mode,
            platform,
        )?;
        let _ = authority_prefix;
        obligations.push(LogicalObligation { key, observables });
    }
    obligations.sort_by(|left, right| left.key.cmp(&right.key));
    validate_catalog(
        &obligations
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok(obligations)
}

/// Splits one logical identifier into〈case, configuration, observables〉.
///
/// The returned `case` still carries the authority prefix for TS/test262
/// identifiers so callers can make the strip decision themselves.
fn parse_logical_identifier(
    catalog: &str,
    identifier: &str,
) -> Result<(String, String, BTreeSet<String>)> {
    let reject = || {
        VerificationError::new(
            ErrorCode::Schema,
            format!("catalog `{catalog}` has a malformed logical identifier `{identifier}`"),
        )
    };
    if catalog.starts_with("typescript-") || catalog == "test262" {
        let parts: Vec<&str> = identifier.split('#').collect();
        let [case, configuration, observable] = parts.as_slice() else {
            return Err(reject());
        };
        if case.is_empty() || configuration.is_empty() || observable.is_empty() {
            return Err(reject());
        }
        let authority_prefix = format!("{catalog}/");
        if !case.starts_with(&authority_prefix) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "logical identifier `{identifier}` does not name the `{catalog}` authority"
                ),
            ));
        }
        return Ok((
            (*case).to_owned(),
            format!("{configuration}#{observable}"),
            BTreeSet::from([(*observable).to_owned()]),
        ));
    }
    if identifier.contains("::") {
        let segments: Vec<&str> = identifier.split("::").collect();
        let [locator, name] = segments.as_slice() else {
            return Err(reject());
        };
        if locator.is_empty() || name.is_empty() {
            return Err(reject());
        }
        return Ok((
            (*locator).to_owned(),
            (*name).to_owned(),
            BTreeSet::from([(*name).to_owned()]),
        ));
    }
    if identifier.is_empty() {
        return Err(reject());
    }
    Ok((
        identifier.to_owned(),
        "default".to_owned(),
        BTreeSet::from([identifier.to_owned()]),
    ))
}

fn load_obligation_classifications(
    root: &Path,
    catalog_name: &str,
    identifiers: &[String],
    obligations: &[LogicalObligation],
) -> Result<BTreeMap<ObligationKey, ClassificationState>> {
    let policy = root
        .join(schema::CLASSIFICATION_DIR)
        .join(format!("{catalog_name}.toml"));
    if !policy.exists() {
        return Ok(BTreeMap::new());
    }
    let cells: Vec<CatalogCell> = match catalog_name {
        "test262" => catalog::extract_test262_cells(
            &root.join("target").join("authority").join("test262"),
            catalog_name,
        )?,
        _ => {
            return Err(VerificationError::new(
                ErrorCode::ToolMissing,
                format!(
                    "{}: classification policy exists but no exact catalog-cell loader is registered",
                    policy.display()
                ),
            ));
        }
    };
    let extracted: Vec<String> = cells.iter().map(CatalogCell::rendered_identity).collect();
    if extracted != identifiers {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "classification universe for `{catalog_name}` does not match the locked manifest"
            ),
        ));
    }
    let mut universes = BTreeMap::new();
    universes.insert(catalog_name.to_owned(), cells);
    let states = classification::load_classifications(root, &universes)?;
    if obligations.len() != identifiers.len() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            "logical obligations do not cover the classification universe",
        ));
    }
    identifiers
        .iter()
        .zip(obligations)
        .map(|(identifier, obligation)| {
            states
                .get(identifier)
                .copied()
                .map(|state| (obligation.key.clone(), state))
                .ok_or_else(|| {
                    VerificationError::new(
                        ErrorCode::SetMismatch,
                        format!("classification state is missing for `{identifier}`"),
                    )
                })
        })
        .collect()
}

/// Executor backing: an internal adapter, an injected lane worker, or a typed
/// unavailable adapter. An unavailable adapter is evidence, not a suite abort.
enum SuiteAdapter {
    TargetCells(BTreeMap<String, crate::toolchain_schema::TargetCellRecord>),
    External(ProcessExecutor),
    Missing(String),
}

impl SuiteAdapter {
    fn build(root: &Path, catalog: &str, runner: SuiteRunner) -> Result<Self> {
        if runner == SuiteRunner::Aot && catalog == "target-cells" {
            return Ok(Self::TargetCells(load_target_cells(root)?));
        }

        let variable = format!(
            "BAMTS_SUITE_{}_ADAPTER",
            runner.as_str().to_ascii_uppercase().replace('-', "_")
        );
        let Some(program) = std::env::var_os(&variable).filter(|value| !value.is_empty()) else {
            return Ok(Self::Missing(format!(
                "runner `{}` has no registered adapter; set `{variable}` to an explicit lane-worker executable",
                runner.as_str()
            )));
        };
        let program = PathBuf::from(program);
        let program = if program.is_absolute() {
            program
        } else {
            root.join(program)
        };
        if !program.is_file() {
            return Ok(Self::Missing(format!(
                "runner `{}` adapter `{}` is not a file",
                runner.as_str(),
                program.display()
            )));
        }
        Ok(Self::External(
            ProcessExecutor::new(program, Vec::new(), root, root.join("target/suite-lanes"))
                .with_max_output_bytes(PROBE_OUTPUT_BYTES),
        ))
    }

    /// Executes one obligation through an internal path. Result is a worker
    /// outcome; the parent — never this function — derives PASS from it.
    fn evaluate(&self, request: &LaneRequest) -> Result<LaneOutcome> {
        match self {
            Self::TargetCells(cells) => {
                let record = cells.get(request.key().case()).ok_or_else(|| {
                    VerificationError::new(
                        ErrorCode::Schema,
                        format!(
                            "no target-cell record for `{}` obligation `{}`",
                            request.key().case(),
                            request.key()
                        ),
                    )
                })?;
                let obligation = record
                    .obligations()
                    .get(request.key().configuration())
                    .ok_or_else(|| {
                        VerificationError::new(
                            ErrorCode::Schema,
                            format!(
                                "target cell `{}` has no `{}` obligation evidence",
                                request.key().case(),
                                request.key().configuration()
                            ),
                        )
                    })?;
                Ok(match obligation.status() {
                    TerminalState::Pass => LaneOutcome::Completed {
                        artifacts: request
                            .observables()
                            .iter()
                            .map(|observable| {
                                (
                                    observable.clone(),
                                    schema::sha256_hex(obligation.evidence().as_bytes()),
                                )
                            })
                            .collect(),
                    },
                    TerminalState::BlockingFail => LaneOutcome::BlockingFail {
                        detail: format!(
                            "{}; missing artifact `{}`",
                            obligation.reason(),
                            obligation.missing_artifact()
                        ),
                    },
                    TerminalState::ExternalBlocked => LaneOutcome::ExternalBlocked {
                        detail: format!(
                            "{}; host must supply `{}`",
                            obligation.reason(),
                            obligation.missing_artifact()
                        ),
                    },
                    TerminalState::InapplicableOutOfScopeHostFeature => {
                        LaneOutcome::InapplicableOutOfScopeHostFeature {
                            detail: obligation.reason().to_owned(),
                        }
                    }
                    other => LaneOutcome::BlockingFail {
                        detail: format!(
                            "target-cell obligation `{}` records non-lane terminal state `{}`",
                            request.key().configuration(),
                            other.as_str()
                        ),
                    },
                })
            }
            Self::Missing(detail) => Ok(LaneOutcome::BlockingFail {
                detail: detail.clone(),
            }),
            Self::External(_) => Err(VerificationError::new(
                ErrorCode::Schema,
                "external lane adapter reached the internal evaluator",
            )),
        }
    }
}

/// Parent-side lane driver over suite adapters and locked classifications.
struct SuiteExecutor {
    adapter: SuiteAdapter,
    classifications: BTreeMap<ObligationKey, ClassificationState>,
}

impl LaneExecutor for SuiteExecutor {
    fn run(&mut self, request: &LaneRequest) -> Result<LaneProcessResult> {
        if let Some(ClassificationState::NonPass(state)) = self.classifications.get(request.key()) {
            return lane_result(
                request,
                classified_outcome(*state, request.key()),
                Instant::now(),
            );
        }
        if let SuiteAdapter::External(executor) = &mut self.adapter {
            return executor.run(request);
        }
        let started = Instant::now();
        let outcome = self.adapter.evaluate(request)?;
        lane_result(request, outcome, started)
    }
}

fn lane_result(
    request: &LaneRequest,
    outcome: LaneOutcome,
    started: Instant,
) -> Result<LaneProcessResult> {
    let response = LaneResponse::new(
        request.binding().clone(),
        request.request_id(),
        request.key().clone(),
        outcome,
    )?;
    let body = serde_json::to_vec(&response).map_err(|error| {
        VerificationError::new(
            ErrorCode::Json,
            format!(
                "cannot encode lane response for `{}`: {error}",
                request.key()
            ),
        )
    })?;
    Ok(LaneProcessResult {
        observation: ProcessObservation::Exited { code: 0 },
        response_body: Some(body),
        duration_ms: started.elapsed().as_millis() as u64,
        detail: String::new(),
    })
}

fn classified_outcome(state: NonPassState, key: &ObligationKey) -> LaneOutcome {
    let detail = format!("locked classification for `{key}`");
    match state {
        NonPassState::BlockingFail => LaneOutcome::BlockingFail { detail },
        NonPassState::InapplicableLanguageService => {
            LaneOutcome::InapplicableLanguageService { detail }
        }
        NonPassState::InapplicableOutOfScopeHostFeature => {
            LaneOutcome::InapplicableOutOfScopeHostFeature { detail }
        }
        NonPassState::InapplicableV8Internal => LaneOutcome::InapplicableV8Internal { detail },
        NonPassState::InapplicableCatalogError => LaneOutcome::InapplicableCatalogError { detail },
        NonPassState::ExternalBlocked => LaneOutcome::ExternalBlocked { detail },
    }
}

/// Runs one catalog shard and publishes a strict current receipt.
///
/// Abort error (manifest, adapter, digests, writer) leaves no receipt
/// behind; per-obligation blocking outcomes never abort the loop.
pub fn run_suite(root: &Path, request: &SuiteRunRequest) -> Result<SuiteReport> {
    let runner = resolve_runner(&request.catalog, &request.runner)?;
    let platform = if request.platform.is_empty() {
        default_platform()
    } else {
        request.platform.clone()
    };
    if platform.trim() != platform || platform.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            "suite platform must be a nonempty canonical token",
        ));
    }
    let catalog = load_catalog_manifest(root, &request.catalog)?;
    let obligations =
        materialize_obligations(&request.catalog, &catalog.identifiers, runner, &platform)?;
    let classifications = load_obligation_classifications(
        root,
        &request.catalog,
        &catalog.identifiers,
        &obligations,
    )?;
    let adapter = SuiteAdapter::build(root, &request.catalog, runner)?;
    let keys: Vec<ObligationKey> = obligations.iter().map(|entry| entry.key.clone()).collect();
    let shard = ShardIdentity::plan(request.shard, &keys)?;
    let binding = current_run_binding(root, &request.catalog)?;
    let header = EvidenceHeader::new(shard.clone(), binding)?;
    let members: Vec<usize> = request.shard.member_indices(keys.len()).collect();
    let lane_binding = {
        let run = LaneBinding::fresh()?;
        if runner == SuiteRunner::Compiler {
            let verified = verify_snapshot(&root.join(DEFAULT_SNAPSHOT_REL))?;
            verified.bind_compiler_lane(run)?
        } else {
            run
        }
    };

    let executor = SuiteExecutor {
        adapter,
        classifications,
    };
    let temp = temp_sibling(&request.receipt);
    if let Some(parent) = request
        .receipt
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| io_path(parent, error))?;
    }
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    let outcome = (|| {
        let file = File::create(&temp).map_err(|error| io_path(&temp, error))?;
        let mut writer = EvidenceWriter::new(BufWriter::new(file), header)?;
        let mut executor = executor;
        for (ordinal, index) in members.iter().enumerate() {
            let obligation = &obligations[*index];
            let lane_request = LaneRequest::new(
                lane_binding.clone(),
                (ordinal + 1) as u64,
                obligation.key.clone(),
                obligation.observables.clone(),
                vec![
                    "bamts-verification::suite::worker".to_owned(),
                    obligation.key.to_string(),
                ],
                WorkingDirectoryPolicy::RepositoryRoot,
                CASE_TIMEOUT_MS,
            )?;
            let process = match executor.run(&lane_request) {
                Ok(process) => process,
                Err(error) => LaneProcessResult {
                    observation: ProcessObservation::Exited { code: 1 },
                    response_body: None,
                    duration_ms: 0,
                    detail: error.to_string(),
                },
            };
            // PASS exists only inside `derive_row`; the suite cannot mint it.
            let row = derive_row(&lane_request, process)?;
            *states.entry(row.state().as_str().to_owned()).or_default() += 1;
            writer.write_row(&row)?;
        }
        writer.finish()?;
        File::open(&temp)
            .map_err(|error| io_path(&temp, error))?
            .sync_all()
            .map_err(|error| io_path(&temp, error))
    })();
    match outcome {
        Ok(()) => {
            fs::rename(&temp, &request.receipt)
                .map_err(|error| io_path(&request.receipt, error))?;
            Ok(SuiteReport {
                catalog: request.catalog.clone(),
                runner: runner.as_str().to_owned(),
                platform: platform.clone(),
                obligations: keys.len(),
                rows: members.len(),
                states,
                obligation_set_digest: shard.obligation_set_digest().to_owned(),
                out: request.receipt.clone(),
                shard: request.shard,
                documents: 0,
            })
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Merges every shard document below `receipts_root` for the catalog.
///
/// Discovery is bounded and rejects wrong catalog, mixed runner/platform,
/// stale or wrong shard identities, mixed run bindings, and empty receipts.
/// The ordered interleave is delegated to [`merge_shards`], which keeps one
/// live row per shard and rejects missing/extra/duplicate rows.
pub fn merge_suite(root: &Path, request: &SuiteMergeRequest) -> Result<SuiteReport> {
    let catalog = request.catalog.as_str();
    let receipts_root = request.receipts.as_path();
    let out = request.out.as_path();
    let mode = request.publish;
    let _ = allowed_runners(catalog)?;
    let mut paths = Vec::new();
    discover_jsonl(receipts_root, &mut paths)?;
    paths.sort();
    if paths.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Io,
            format!(
                "{}: no `.jsonl` receipts found below the receipts root",
                receipts_root.display()
            ),
        ));
    }

    struct ShardProbe {
        path: PathBuf,
        header: EvidenceHeader,
        first_row: EvidenceRow,
    }

    let mut probes: Vec<ShardProbe> = Vec::new();
    for path in &paths {
        let mut reader = EvidenceReader::open(path)?;
        let Some(first_row) = reader.next_row()? else {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("{}: receipt carries no obligation rows", path.display()),
            ));
        };
        let header = reader.header().clone();
        let _footer = reader.finish()?;
        if first_row.key().catalog() != catalog {
            // A workflow may pass the shared receipts root; only complete,
            // internally valid documents for other catalogs are ignored.
            continue;
        }
        probes.push(ShardProbe {
            path: path.clone(),
            header,
            first_row,
        });
    }
    if probes.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "{}: no receipts for catalog `{catalog}`",
                receipts_root.display()
            ),
        ));
    }

    let modes: BTreeSet<crate::shard::ExecutionMode> = probes
        .iter()
        .map(|probe| probe.first_row.key().mode())
        .collect();
    let platforms: BTreeSet<String> = probes
        .iter()
        .map(|probe| probe.first_row.key().platform().to_owned())
        .collect();
    if modes.len() != 1 || platforms.len() != 1 {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "receipts for catalog `{catalog}` mix runner modes or platforms ({modes:?}, {platforms:?})"
            ),
        ));
    }
    let mode_value = *modes.iter().next().expect("nonempty mode set");
    let platform = platforms.iter().next().expect("nonempty platform set");
    let runner = runner_for_mode(catalog, mode_value).ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!(
                "receipts bind runner mode `{mode_value:?}` for catalog `{catalog}`, which no workflow declares"
            ),
        )
    })?;

    let binding = probes[0].header.binding().clone();
    for probe in &probes[1..] {
        if binding != *probe.header.binding() {
            return Err(VerificationError::new(
                ErrorCode::Digest,
                format!(
                    "{}: run binding differs from sibling shard receipts (mixed authority, candidate, or harness digests)",
                    probe.path.display()
                ),
            ));
        }
    }

    let manifest = load_catalog_manifest(root, catalog)?;
    let obligations = materialize_obligations(catalog, &manifest.identifiers, runner, platform)?;
    let _classifications =
        load_obligation_classifications(root, catalog, &manifest.identifiers, &obligations)?;
    let current_binding = current_run_binding(root, catalog)?;
    if binding != current_binding {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            "shard receipts are stale for the current authority, candidate, or harness binding",
        ));
    }
    let keys: Vec<ObligationKey> = obligations.iter().map(|entry| entry.key.clone()).collect();
    let mut shard_count = None;
    let mut shard_indices = BTreeSet::new();
    for probe in &probes {
        let spec = probe.header.shard().spec();
        match shard_count {
            None => shard_count = Some(spec.count()),
            Some(count) if count != spec.count() => {
                return Err(VerificationError::new(
                    ErrorCode::SetMismatch,
                    "shard receipts do not share one matrix count",
                ));
            }
            Some(_) => {}
        }
        if !shard_indices.insert(spec.index()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate shard index {}", spec.index()),
            ));
        }
        let expected = ShardIdentity::plan(spec, &keys)?;
        if expected != *probe.header.shard() {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "{}: shard identity is stale or foreign to the current catalog `{catalog}`",
                    probe.path.display()
                ),
            ));
        }
    }
    let count = shard_count.expect("nonempty probes have a count");
    if probes.len() != count as usize || shard_indices.len() != count as usize {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            "merge requires every shard index exactly once",
        ));
    }

    let shard_paths: Vec<PathBuf> = probes.iter().map(|probe| probe.path.clone()).collect();
    merge_shards(&shard_paths, &keys, out, mode)?;

    // Bounded closure proof: the published image streams again and its
    // footer must account for exactly the canonical catalog.
    let mut reader = EvidenceReader::open(out)?;
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    while let Some(row) = reader.next_row()? {
        *states.entry(row.state().as_str().to_owned()).or_default() += 1;
    }
    let footer = reader.finish()?;
    if footer.row_count() != keys.len() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "merged receipt records {} rows for a catalog of {}",
                footer.row_count(),
                keys.len()
            ),
        ));
    }
    Ok(SuiteReport {
        catalog: catalog.to_owned(),
        runner: runner.as_str().to_owned(),
        platform: platform.clone(),
        obligations: keys.len(),
        rows: footer.row_count(),
        states,
        obligation_set_digest: crate::shard::digest_obligation_set(keys.iter()),
        out: out.to_path_buf(),
        shard: ShardSpec::unsharded(),
        documents: shard_paths.len(),
    })
}

/// Authority directories under `target/authority` that this catalog reads.
fn authority_dirs(catalog: &str) -> &'static [&'static str] {
    match catalog {
        "typescript-7.0.2" => &["typescript-7.0.2", "typescript-7.0.2-tests"],
        "typescript-6.0.2" => &["typescript-6.0.2-tests"],
        "typescript-5.9.3" => &["typescript-5.9.3-tests"],
        "test262" => &["test262"],
        _ => &[],
    }
}

#[derive(Debug, Deserialize)]
struct SourceMarker {
    name: String,
    tree_digest: String,
}

/// Binds the locked authority materialization and classification policy.
fn authority_digest(root: &Path, catalog: &str) -> Result<String> {
    let dirs = authority_dirs(catalog);
    let mut records: Vec<(String, String)> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let marker_path = root
            .join("target")
            .join("authority")
            .join(dir)
            .join(".bamti-source.json");
        let bytes = schema::read_bytes(&marker_path).map_err(|error| {
            VerificationError::new(
                ErrorCode::ToolMissing,
                format!(
                    "authority `{dir}` is not materialized under target/authority ({error}); run `source fetch {dir} --dest target/authority/{dir}` first"
                ),
            )
        })?;
        let marker: SourceMarker = serde_json::from_slice(&bytes).map_err(|error| {
            VerificationError::new(
                ErrorCode::Json,
                format!("{}: invalid source marker: {error}", marker_path.display()),
            )
        })?;
        records.push((marker.name, marker.tree_digest));
    }
    records.sort();
    let mut hasher = Sha256::new();
    for (name, tree_digest) in records {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(tree_digest.as_bytes());
        hasher.update([0x0a]);
    }
    let policy = root
        .join(schema::CLASSIFICATION_DIR)
        .join(format!("{catalog}.toml"));
    hasher.update(b"classification\x00");
    if policy.exists() {
        hasher.update(file_sha256(&policy)?.as_bytes());
    } else {
        hasher.update(schema::sha256_hex(b"").as_bytes());
    }
    hasher.update([0x0a]);
    Ok(schema::sha256_hex(&hasher.finalize()))
}

/// SHA-256 of one file.
fn file_sha256(path: &Path) -> Result<String> {
    let bytes = schema::read_bytes(path)?;
    Ok(schema::sha256_hex(&bytes))
}

/// Runs `git` through the bounded corpus process boundary.
fn git_probe(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let invocation = oracles::ProcessInvocation {
        program: PathBuf::from("git"),
        argv: args.iter().map(|arg| (*arg).into()).collect(),
        cwd: root.to_path_buf(),
        environment: oracles::pinned_environment(),
        limits: crate::corpus::OracleLimits {
            timeout: PROBE_TIMEOUT,
            max_output_bytes: PROBE_OUTPUT_BYTES,
        },
    };
    let outcome = oracles::CorpusProcessBoundary
        .invoke(&invocation)
        .map_err(|error| {
            VerificationError::new(
                ErrorCode::ToolFailed,
                format!("git probe `git {}` failed: {error}", args.join(" ")),
            )
        })?;
    if outcome.exit_code != Some(0) {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "git probe `git {}` exited {:?}",
                args.join(" "),
                outcome.exit_code
            ),
        ));
    }
    Ok(outcome.stdout)
}

/// Candidate tree identity: the HEAD tree hash, folded with every dirty or
/// untracked candidate file's current bytes.
fn candidate_tree_digest(root: &Path) -> Result<String> {
    let tree = git_probe(root, &["rev-parse", "HEAD^{tree}"])?;
    let tree = String::from_utf8_lossy(&tree).trim().to_owned();
    if !matches!(tree.len(), 40 | 64)
        || !tree
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!("git reported a malformed tree digest `{tree}`"),
        ));
    }
    let mut clean_hasher = Sha256::new();
    clean_hasher.update(b"git-tree\x00");
    clean_hasher.update(tree.as_bytes());
    let clean = schema::sha256_hex(&clean_hasher.finalize());
    let status = git_probe(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if status.is_empty() {
        return Ok(clean);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"dirty-tree\x00");
    hasher.update(tree.as_bytes());
    hasher.update([0x0a]);
    let mut fields = status
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 || field[2] != b' ' {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                "git produced malformed porcelain status",
            ));
        }
        let renamed = matches!(field[0], b'R' | b'C') || matches!(field[1], b'R' | b'C');
        let deleted = field[0] == b'D' || field[1] == b'D';
        let relative = std::str::from_utf8(&field[3..]).map_err(|_| {
            VerificationError::new(ErrorCode::Schema, "git status path is not UTF-8")
        })?;
        hasher.update(field);
        hasher.update([0]);
        if !deleted {
            hash_candidate_path(root, Path::new(relative), &mut hasher)?;
        }
        if renamed {
            let source = fields.next().ok_or_else(|| {
                VerificationError::new(ErrorCode::Schema, "git rename status lacks its source path")
            })?;
            hasher.update(source);
            hasher.update([0]);
        }
    }
    Ok(schema::sha256_hex(&hasher.finalize()))
}

fn hash_candidate_path(root: &Path, relative: &Path, hasher: &mut Sha256) -> Result<()> {
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_path(&path, error)),
    };
    if metadata.file_type().is_symlink() {
        hasher.update(
            fs::read_link(&path)
                .map_err(|error| io_path(&path, error))?
                .as_os_str()
                .as_encoded_bytes(),
        );
        hasher.update([0x0a]);
        return Ok(());
    }
    // Porcelain already binds the directory path and status; it has no file bytes to hash.
    if metadata.is_dir() {
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "dirty candidate path `{}` is not a file",
                relative.display()
            ),
        ));
    }
    let mut file = File::open(&path).map_err(|error| io_path(&path, error))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_path(&path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.update([0x0a]);
    Ok(())
}

#[derive(Debug)]
struct RunSnapshot {
    candidate_tree_digest: String,
    candidate_binary_digest: String,
    harness_digest: String,
}

fn current_run_snapshot(root: &Path) -> Result<RunSnapshot> {
    let candidate_tree_digest = candidate_tree_digest(root)?;
    let exe = std::env::current_exe().map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot resolve the harness binary: {error}"),
        )
    })?;
    // Wired runners execute the candidate in-process; the harness binary
    // contains the candidate libraries, so both digests name it.
    let candidate_binary_digest = file_sha256(&exe)?;
    Ok(RunSnapshot {
        candidate_tree_digest,
        harness_digest: candidate_binary_digest.clone(),
        candidate_binary_digest,
    })
}

fn binding_from_snapshot(root: &Path, catalog: &str, snapshot: &RunSnapshot) -> Result<RunBinding> {
    RunBinding::new(
        authority_digest(root, catalog)?,
        snapshot.candidate_tree_digest.clone(),
        snapshot.candidate_binary_digest.clone(),
        snapshot.harness_digest.clone(),
    )
}

/// The four digests + normalized environment the header binds.
pub fn current_run_binding(root: &Path, catalog: &str) -> Result<RunBinding> {
    binding_from_snapshot(root, catalog, &current_run_snapshot(root)?)
}

/// Current bindings for every catalog, sharing one exact run snapshot.
pub fn current_run_bindings(
    root: &Path,
    catalogs: &BTreeSet<String>,
) -> Result<BTreeMap<String, RunBinding>> {
    let snapshot = current_run_snapshot(root)?;
    catalogs
        .iter()
        .map(|catalog| {
            binding_from_snapshot(root, catalog, &snapshot)
                .map(|binding| (catalog.clone(), binding))
        })
        .collect()
}

/// Destination-sibling temp path used before the atomic rename.
fn temp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "receipt".into());
    name.push(format!(".tmp-{}", std::process::id()));
    dest.with_file_name(name)
}

fn io_path(path: &Path, error: std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

/// Collects `.jsonl` files below `root`, recursively, deterministic order.
fn discover_jsonl(root: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(root).map_err(|error| io_path(root, error))?;
    let mut entries = entries
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| io_path(root, error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| io_path(&path, error))?;
        if file_type.is_dir() {
            discover_jsonl(&path, found)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            found.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::ExecutionMode;

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "bamts-suite-test-{}-{label}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("scratch root");
            Self { root }
        }

        fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("scratch parent");
            }
            fs::write(&path, bytes).expect("scratch write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn hex(byte: u8) -> String {
        std::iter::repeat_n(char::from_digit(byte as u32, 16).expect("hex digit"), 64).collect()
    }

    fn manifest_bytes(
        identifiers: &[&str],
        source_ledger: &str,
        identifiers_sha256: &str,
    ) -> Vec<u8> {
        let mut sorted: Vec<String> = identifiers
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect();
        sorted.sort();
        let list = sorted
            .iter()
            .map(|entry| format!("\"{entry}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{{\"schema\": \"{MANIFEST_SCHEMA}\", \"source_ledger_sha256\": \"{source_ledger}\", \
             \"catalogs\": [{{\"extractor\": {{}}, \"id\": \"benchmarks\", \
             \"identifier_count\": {}, \"identifiers\": [{}], \
             \"identifiers_sha256\": \"{}\", \
             \"source\": {{\"pin\": \"p\", \"url\": \"u\", \"digest_algorithm\": \"sha256\", \
             \"digest\": \"{}\"}}}}]}}",
            sorted.len(),
            list,
            identifiers_sha256,
            hex(3)
        )
        .into_bytes()
    }

    fn manifest_root(label: &str, identifiers: &[&str]) -> (Scratch, Vec<String>) {
        let scratch = Scratch::new(label);
        let sources = b"[[source]]\n";
        let source_ledger = schema::sha256_hex(sources);
        let mut sorted: Vec<String> = identifiers
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect();
        sorted.sort();
        let digest = schema::identifiers_sha256(&sorted);
        scratch.write("vendor/sources.toml", sources);
        scratch.write(
            "verification/manifest.lock.json",
            &manifest_bytes(identifiers, &source_ledger, &digest),
        );
        scratch.write(
            ".gitignore",
            b"receipts/\npartial/\nstale/\nextra/\nwrong-mode/\ntwo/\nduplicate/\n*.jsonl\nout*.jsonl\n",
        );
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&scratch.root)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?}");
        };
        run_git(&["init", "-q"]);
        run_git(&["add", "."]);
        run_git(&[
            "-c",
            "user.name=bamts-suite-test",
            "-c",
            "user.email=suite@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        (scratch, sorted)
    }

    #[test]
    fn current_run_bindings_scope_authority_but_share_run_identity() {
        let (scratch, _) = manifest_root("binding-map", &["jit.a"]);
        scratch.write(
            "verification/classification/catalog-a.toml",
            b"catalog = \"a\"\n",
        );
        scratch.write(
            "verification/classification/catalog-b.toml",
            b"catalog = \"b\"\n",
        );
        let catalogs = BTreeSet::from(["catalog-b".to_owned(), "catalog-a".to_owned()]);

        let bindings = current_run_bindings(&scratch.root, &catalogs).expect("bindings");
        assert_eq!(
            bindings.keys().map(String::as_str).collect::<Vec<_>>(),
            ["catalog-a", "catalog-b"]
        );
        let first = &bindings["catalog-a"];
        let second = &bindings["catalog-b"];
        assert_ne!(first.authority_digest(), second.authority_digest());
        assert!(first.same_run_as(second));
    }

    #[test]
    fn tracked_deletions_bind_status_and_path_without_file_bytes() {
        for (label, staged, expected_status) in [
            ("worktree-deletion", false, b" D tracked.txt\0"),
            ("index-deletion", true, b"D  tracked.txt\0"),
        ] {
            let (scratch, _) = manifest_root(label, &["jit.a"]);
            let tracked = scratch.write("tracked.txt", b"tracked bytes");
            let run_git = |args: &[&str]| {
                let status = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&scratch.root)
                    .status()
                    .expect("run git");
                assert!(status.success(), "git {args:?}");
            };
            run_git(&["add", "tracked.txt"]);
            run_git(&[
                "-c",
                "user.name=bamts-suite-test",
                "-c",
                "user.email=suite@example.invalid",
                "commit",
                "-qm",
                "tracked deletion fixture",
            ]);
            let clean = candidate_tree_digest(&scratch.root).expect("clean candidate digest");

            if staged {
                run_git(&["rm", "-q", "tracked.txt"]);
            } else {
                fs::remove_file(&tracked).expect("delete tracked file");
            }
            assert!(!tracked.exists(), "deleted path must have no bytes to hash");
            let status = git_probe(
                &scratch.root,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            )
            .expect("deletion status");
            assert_eq!(status, expected_status.as_slice());

            let dirty = candidate_tree_digest(&scratch.root).expect("deleted candidate digest");
            assert_ne!(dirty, clean);
            assert_eq!(
                candidate_tree_digest(&scratch.root).expect("repeat deleted candidate digest"),
                dirty
            );
        }
    }

    #[test]
    fn embedded_git_directory_contributes_status_without_file_bytes() {
        let (scratch, _) = manifest_root("embedded-git", &["jit.a"]);
        let clean = candidate_tree_digest(&scratch.root).expect("clean candidate digest");
        let embedded = scratch.root.join(".references/bun");
        fs::create_dir_all(&embedded).expect("create embedded repository");
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&embedded)
            .status()
            .expect("initialize embedded repository");
        assert!(status.success());

        let status = git_probe(
            &scratch.root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .expect("embedded repository status");
        assert!(String::from_utf8_lossy(&status).contains(".references/bun/"));

        let dirty = candidate_tree_digest(&scratch.root)
            .expect("embedded repository is a non-file candidate");
        assert_ne!(dirty, clean);
    }

    #[test]
    fn manifest_identity_parsing_is_strict() {
        let (scratch, sorted) = manifest_root("identity", &["jit.c", "jit.a", "jit.b"]);
        let loaded = load_catalog_manifest(&scratch.root, "benchmarks").expect("loads");
        assert_eq!(loaded.catalog, "benchmarks");
        assert_eq!(loaded.identifiers, sorted);
        assert_eq!(
            loaded.identifiers_sha256,
            schema::identifiers_sha256(&sorted)
        );
        assert_eq!(loaded.manifest_sha256.len(), 64);

        // Sources-ledger drift is rejected as stale, not parsed over.
        scratch.write("vendor/sources.toml", b"[[source]]\n# drift\n");
        let error = load_catalog_manifest(&scratch.root, "benchmarks")
            .expect_err("stale sources ledger must fail");
        assert_eq!(error.code(), ErrorCode::Schema);

        let (mutated, _) = manifest_root("identity-dup", &["jit.a", "jit.b"]);
        let bytes = fs::read(mutated.root.join(schema::MANIFEST_PATH)).expect("read");
        let text = String::from_utf8(bytes)
            .expect("utf8")
            .replacen("\"jit.b\"", "\"jit.a\"", 1);
        // Duplicate JSON keys are caught by the structural check; the
        // duplicate identifier itself is caught by the digest.
        assert!(text.contains("\"jit.a\", \"jit.a\""));
        fs::write(mutated.root.join(schema::MANIFEST_PATH), text).expect("write");
        assert!(load_catalog_manifest(&mutated.root, "benchmarks").is_err());

        let (bad_tag, _) = manifest_root("identity-tag", &["jit.a"]);
        let bytes = fs::read(bad_tag.root.join(schema::MANIFEST_PATH)).expect("read");
        let text = String::from_utf8(bytes)
            .expect("utf8")
            .replace(MANIFEST_SCHEMA, "bamti.other/v9");
        fs::write(bad_tag.root.join(schema::MANIFEST_PATH), text).expect("write");
        assert!(load_catalog_manifest(&bad_tag.root, "benchmarks").is_err());
    }

    #[test]
    fn runner_allowlist_accepts_declared_and_rejects_mismatch() {
        let declared = [
            ("typescript-7.0.2", "compiler", SuiteRunner::Compiler),
            ("typescript-6.0.2", "compiler", SuiteRunner::Compiler),
            ("test262", "interpreter", SuiteRunner::Interpreter),
            ("test262", "jit", SuiteRunner::Jit),
            ("test262", "aot", SuiteRunner::Aot),
            ("formal-quint", "quint", SuiteRunner::Quint),
            ("formal-lean", "lean", SuiteRunner::Lean),
            ("formal-redex", "redex", SuiteRunner::Redex),
            ("target-cells", "aot", SuiteRunner::Aot),
            ("benchmarks", "perf", SuiteRunner::Perf),
        ];
        for (catalog, runner, expected) in declared {
            assert_eq!(
                resolve_runner(catalog, runner).expect("declared"),
                expected,
                "{catalog}/{runner}"
            );
        }
        for (catalog, runner) in [
            ("typescript-7.0.2", "aot"),
            ("typescript-7.0.2", "interpreter"),
            ("test262", "compiler"),
            ("test262", "perf"),
            ("formal-quint", "lean"),
            ("formal-lean", "quint"),
            ("formal-redex", "compiler"),
            ("target-cells", "perf"),
            ("benchmarks", "aot"),
        ] {
            let error = resolve_runner(catalog, runner).expect_err("undeclared runner must fail");
            assert_eq!(error.code(), ErrorCode::Schema, "{catalog}/{runner}");
            let text = error.to_string();
            assert!(text.contains(catalog) && text.contains(runner), "{text}");
        }
        assert_eq!(
            resolve_runner("benchmarks", "").expect("sole runner"),
            SuiteRunner::Perf
        );
        assert!(resolve_runner("test262", "").is_err());
        assert_eq!(
            resolve_runner("node-24", "compiler")
                .expect_err("foreign catalog")
                .code(),
            ErrorCode::Usage
        );
        assert_eq!(
            resolve_runner("benchmarks", "Perf")
                .expect_err("case mismatch")
                .code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn shard_membership_partitions_the_canonical_catalog() {
        let identifiers = [
            "jit.a", "jit.b", "jit.c", "jit.d", "jit.e", "jit.f", "jit.g",
        ];
        let obligations = materialize_obligations(
            "benchmarks",
            &identifiers
                .iter()
                .map(|entry| (*entry).to_owned())
                .collect::<Vec<_>>(),
            SuiteRunner::Perf,
            "ubuntu-latest",
        )
        .expect("obligations");
        assert_eq!(obligations.len(), 7);
        for count in [1usize, 2, 3, 5, 7] {
            let mut seen: BTreeSet<usize> = BTreeSet::new();
            for index in 0..count {
                let spec = ShardSpec::new(index as u32, count as u32).expect("spec");
                for member in spec.member_indices(obligations.len()) {
                    assert!(spec.owns(member));
                    assert!(seen.insert(member), "shards must be disjoint");
                }
            }
            assert_eq!(seen.len(), obligations.len(), "shards must cover");
        }
        let keys: Vec<ObligationKey> = obligations.into_iter().map(|entry| entry.key).collect();
        let head = ShardIdentity::plan(ShardSpec::new(0, 3).expect("spec"), &keys).expect("plan");
        let tail = ShardIdentity::plan(ShardSpec::new(2, 3).expect("spec"), &keys).expect("plan");
        assert_eq!(head.catalog_digest(), tail.catalog_digest());
        assert_ne!(head.obligation_set_digest(), tail.obligation_set_digest());
        assert_eq!(
            head.expected_count() + tail.expected_count() + 3,
            keys.len() + 1
        );
    }

    #[test]
    fn suite_never_mints_pass_without_exact_observables() {
        fn key(case: &str) -> ObligationKey {
            ObligationKey::new(
                "benchmarks",
                case,
                "default",
                ExecutionMode::Aot,
                "ubuntu-latest",
            )
            .expect("key")
        }
        let declared = BTreeSet::from(["jit.a".to_owned()]);
        let binding = LaneBinding::unbound(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("binding");
        let request = LaneRequest::new(
            binding.clone(),
            7,
            key("jit.a"),
            declared,
            vec!["bamts-verification::suite::worker".to_owned()],
            WorkingDirectoryPolicy::RepositoryRoot,
            CASE_TIMEOUT_MS,
        )
        .expect("request");
        let response_completed = |artifacts: BTreeMap<String, String>, request_id: u64| {
            serde_json::to_vec(
                &LaneResponse::new(
                    binding.clone(),
                    request_id,
                    key("jit.a"),
                    LaneOutcome::Completed { artifacts },
                )
                .expect("response"),
            )
            .expect("encode")
        };
        // Claimed completion with the exact declared artifact set, correct
        // binding, request_id, and key: only the parent derivation may turn this into PASS.
        let exact = BTreeMap::from([("jit.a".to_owned(), hex(9))]);
        let row = derive_row(
            &request,
            LaneProcessResult {
                observation: ProcessObservation::Exited { code: 0 },
                response_body: Some(response_completed(exact.clone(), 7)),
                duration_ms: 1,
                detail: String::new(),
            },
        )
        .expect("derive");
        assert_eq!(row.state(), TerminalState::Pass);
        // Same claim with a wrong request_id, a wrong observable, or an extra
        // artifact is protocol evidence, never PASS.
        let mut extra = exact.clone();
        extra.insert("stderr".to_owned(), hex(4));
        for body in [
            response_completed(exact.clone(), 8),
            response_completed(BTreeMap::from([("other".to_owned(), hex(9))]), 7),
            response_completed(extra, 7),
        ] {
            let row = derive_row(
                &request,
                LaneProcessResult {
                    observation: ProcessObservation::Exited { code: 0 },
                    response_body: Some(body),
                    duration_ms: 1,
                    detail: String::new(),
                },
            )
            .expect("derive");
            assert_eq!(row.state(), TerminalState::ProtocolError);
            assert!(!row.state().is_pass());
        }
    }

    fn binding_for_tests(path: &Path) -> RunBinding {
        let repository = path
            .ancestors()
            .find(|candidate| candidate.join(".git").is_dir())
            .expect("fixture git repository");
        current_run_binding(repository, "benchmarks").expect("current binding")
    }

    fn write_shard(root: &Path, name: &str, keys: &[ObligationKey], spec: ShardSpec) -> PathBuf {
        let obligations: Vec<EvidenceRow> = spec
            .member_indices(keys.len())
            .map(|index| {
                let key = &keys[index];
                EvidenceRow::new(
                    key.clone(),
                    vec!["bamts-verification::suite::worker".to_owned()],
                    WorkingDirectoryPolicy::RepositoryRoot,
                    BTreeSet::from([key.configuration().to_owned()]),
                    BTreeMap::new(),
                    TerminalState::BlockingFail,
                    1,
                    "synthetic blocking outcome",
                )
                .expect("row")
            })
            .collect();
        let header = EvidenceHeader::new(
            ShardIdentity::plan(spec, keys).expect("plan"),
            binding_for_tests(root),
        )
        .expect("header");
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("shard parent");
        }
        let file = File::create(&path).expect("shard file");
        let mut writer = EvidenceWriter::new(file, header).expect("writer");
        for row in &obligations {
            writer.write_row(row).expect("write");
        }
        writer.finish().expect("finish");
        path
    }

    fn merge_for_tests(
        root: &Path,
        catalog: &str,
        receipts: &Path,
        out: &Path,
        publish: PublishMode,
    ) -> Result<SuiteReport> {
        merge_suite(
            root,
            &SuiteMergeRequest {
                catalog: catalog.to_owned(),
                receipts: receipts.to_path_buf(),
                out: out.to_path_buf(),
                publish,
            },
        )
    }

    #[test]
    fn merge_closure_rejects_missing_extra_and_stale_rows() {
        let (scratch, sorted) = manifest_root(
            "merge",
            &["jit.a", "jit.b", "jit.c", "jit.d", "jit.e", "jit.f"],
        );
        let root = scratch.root.clone();
        let obligations =
            materialize_obligations("benchmarks", &sorted, SuiteRunner::Perf, "ubuntu-latest")
                .expect("obligations");
        let keys: Vec<ObligationKey> = obligations.into_iter().map(|entry| entry.key).collect();
        let receipts = root.join("receipts");
        for index in 0..3u32 {
            write_shard(
                &receipts,
                &format!("shard-{index}.jsonl"),
                &keys,
                ShardSpec::new(index, 3).expect("spec"),
            );
        }
        let out = root.join("merged.jsonl");
        let report = merge_for_tests(&root, "benchmarks", &receipts, &out, PublishMode::Replace)
            .expect("complete merge");
        assert_eq!(report.rows, keys.len());
        assert_eq!(report.documents, 3);
        assert_eq!(report.platform, "ubuntu-latest");
        assert_eq!(report.runner, "perf");
        assert_eq!(report.states.get("BLOCKING_FAIL"), Some(&keys.len()));
        assert_eq!(
            report.obligation_set_digest,
            crate::shard::digest_obligation_set(keys.iter())
        );

        // A missing shard aborts the merge.
        let partial = root.join("partial");
        fs::create_dir_all(&partial).expect("partial dir");
        for index in 0..2u32 {
            write_shard(
                &partial,
                &format!("shard-{index}.jsonl"),
                &keys,
                ShardSpec::new(index, 3).expect("spec"),
            );
        }
        assert!(
            merge_for_tests(
                &root,
                "benchmarks",
                &partial,
                &root.join("out.jsonl"),
                PublishMode::Replace
            )
            .is_err(),
            "missing shard must be rejected"
        );

        // A stale run binding (other authority digest) aborts the merge.
        let stale = root.join("stale");
        fs::create_dir_all(&stale).expect("stale dir");
        for index in 0..3u32 {
            {
                // Resolve shard rows with a divergent binding.
                let obligations: Vec<EvidenceRow> = ShardSpec::new(index, 3)
                    .expect("spec")
                    .member_indices(keys.len())
                    .map(|case| {
                        EvidenceRow::new(
                            keys[case].clone(),
                            vec![],
                            WorkingDirectoryPolicy::RepositoryRoot,
                            BTreeSet::from([keys[case].configuration().to_owned()]),
                            BTreeMap::new(),
                            TerminalState::BlockingFail,
                            1,
                            "synthetic blocking outcome",
                        )
                        .expect("row")
                    })
                    .collect();
                let binding = if index == 2 {
                    RunBinding::new(hex(9), hex(2), hex(3), hex(4)).expect("binding")
                } else {
                    binding_for_tests(&root)
                };
                let header = EvidenceHeader::new(
                    ShardIdentity::plan(ShardSpec::new(index, 3).expect("spec"), &keys)
                        .expect("plan"),
                    binding,
                )
                .expect("header");
                let file = File::create(stale.join(format!("stale-{index}.jsonl"))).expect("file");
                let mut writer = EvidenceWriter::new(file, header).expect("writer");
                for row in &obligations {
                    writer.write_row(row).expect("write");
                }
                writer.finish().expect("finish");
            };
        }
        let error = merge_for_tests(
            &root,
            "benchmarks",
            &stale,
            &root.join("out2.jsonl"),
            PublishMode::Replace,
        )
        .expect_err("mixed authority binding must fail");
        assert_eq!(error.code(), ErrorCode::Digest);

        // An extra, foreign obligation row breaks closure.
        let extra = root.join("extra");
        fs::create_dir_all(&extra).expect("extra dir");
        for index in 0..3u32 {
            write_shard(
                &extra,
                &format!("shard-{index}.jsonl"),
                &keys,
                ShardSpec::new(index, 3).expect("spec"),
            );
        }
        let foreign = ObligationKey::new(
            "benchmarks",
            "aaa.foreign",
            "default",
            ExecutionMode::Aot,
            "ubuntu-latest",
        )
        .expect("foreign key");
        let mut foreign_keys = keys.clone();
        foreign_keys[0] = foreign;
        write_shard(
            &extra,
            "shard-0.jsonl",
            &foreign_keys,
            ShardSpec::new(0, 3).expect("spec"),
        );
        assert!(
            merge_for_tests(
                &root,
                "benchmarks",
                &extra,
                &root.join("out3.jsonl"),
                PublishMode::Replace
            )
            .is_err(),
            "extra row must be rejected"
        );

        // A runner mode no workflow declares is rejected before merging.
        let wrong_mode = root.join("wrong-mode");
        fs::create_dir_all(&wrong_mode).expect("dir");
        let wrong_keys: Vec<ObligationKey> = sorted
            .iter()
            .map(|identifier| {
                ObligationKey::new(
                    "benchmarks",
                    identifier,
                    "default",
                    ExecutionMode::Jit,
                    "ubuntu-latest",
                )
                .expect("key")
            })
            .collect();
        let header = EvidenceHeader::new(
            ShardIdentity::plan(ShardSpec::new(0, 1).expect("spec"), &wrong_keys).expect("plan"),
            binding_for_tests(&root),
        )
        .expect("header");
        let file = File::create(wrong_mode.join("only.jsonl")).expect("file");
        let mut writer = EvidenceWriter::new(file, header).expect("writer");
        for key in &wrong_keys {
            writer
                .write_row(
                    &EvidenceRow::new(
                        key.clone(),
                        vec![],
                        WorkingDirectoryPolicy::RepositoryRoot,
                        BTreeSet::from(["default".to_owned()]),
                        BTreeMap::new(),
                        TerminalState::BlockingFail,
                        1,
                        "wrong runner mode",
                    )
                    .expect("row"),
                )
                .expect("write");
        }
        writer.finish().expect("finish");
        assert_eq!(
            merge_for_tests(
                &root,
                "benchmarks",
                &wrong_mode,
                &root.join("out4.jsonl"),
                PublishMode::Replace
            )
            .expect_err("undeclared runner mode must fail")
            .code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn two_half_merge_is_deterministic_and_failure_preserves_output() {
        let (scratch, sorted) = manifest_root("two-half", &["jit.a", "jit.b", "jit.c", "jit.d"]);
        let root = scratch.root.clone();
        let obligations =
            materialize_obligations("benchmarks", &sorted, SuiteRunner::Perf, "ubuntu-latest")
                .expect("obligations");
        let keys: Vec<ObligationKey> = obligations.into_iter().map(|entry| entry.key).collect();
        let receipts = root.join("two");
        for index in 0..2 {
            write_shard(
                &receipts,
                &format!("shard-{index}.jsonl"),
                &keys,
                ShardSpec::new(index, 2).expect("spec"),
            );
        }
        let out = root.join("two-halves.jsonl");
        let report = merge_for_tests(&root, "benchmarks", &receipts, &out, PublishMode::Replace)
            .expect("two-half closure");
        assert_eq!(report.documents, 2);
        assert_eq!(report.rows, keys.len());
        let canonical = fs::read(&out).expect("canonical output");
        merge_for_tests(&root, "benchmarks", &receipts, &out, PublishMode::Replace)
            .expect("deterministic replacement");
        assert_eq!(fs::read(&out).expect("replacement"), canonical);

        let duplicate = root.join("duplicate");
        fs::create_dir_all(&duplicate).expect("duplicate root");
        let shard0 = fs::read_to_string(receipts.join("shard-0.jsonl")).expect("shard 0");
        let mut lines: Vec<&str> = shard0.lines().collect();
        lines.insert(2, lines[1]);
        fs::write(
            duplicate.join("shard-0.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .expect("tampered duplicate row");
        fs::copy(
            receipts.join("shard-1.jsonl"),
            duplicate.join("shard-1.jsonl"),
        )
        .expect("shard 1");
        assert!(
            merge_for_tests(&root, "benchmarks", &duplicate, &out, PublishMode::Replace).is_err(),
            "duplicate row must be rejected"
        );
        assert_eq!(
            fs::read(&out).expect("preserved output"),
            canonical,
            "failed merge must not mutate the published output"
        );
    }

    #[test]
    fn missing_adapter_records_typed_blocking_rows_and_continues() {
        assert!(
            std::env::var_os("BAMTS_SUITE_PERF_ADAPTER").is_none(),
            "test requires no injected perf adapter"
        );
        let (scratch, _) = manifest_root("closed", &["jit.a", "jit.b"]);
        let receipt = scratch.root.join("receipt.jsonl");
        let report = run_suite(
            &scratch.root,
            &SuiteRunRequest {
                catalog: "benchmarks".to_owned(),
                shard: ShardSpec::unsharded(),
                receipt: receipt.clone(),
                runner: "perf".to_owned(),
                platform: "ubuntu-latest".to_owned(),
            },
        )
        .expect("missing adapter is blocking evidence, not an abort");
        assert_eq!(report.rows, 2);
        assert_eq!(report.states.get("BLOCKING_FAIL"), Some(&2));

        let mut reader = EvidenceReader::open(&receipt).expect("receipt");
        let mut rows = 0;
        while let Some(row) = reader.next_row().expect("row") {
            assert_eq!(row.state(), TerminalState::BlockingFail);
            rows += 1;
        }
        assert_eq!(reader.finish().expect("footer").row_count(), rows);
        assert_eq!(rows, 2);
    }
}
