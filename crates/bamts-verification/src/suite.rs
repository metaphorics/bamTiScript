//! TypeScript 7 suite importer, snapshot materializer, and bounded runner (U0.4).
//!
//! Import pipeline accepts an extracted tree root for hermetic tests; curl/tar is
//! isolated behind fetch/extract helpers. Snapshot digests are walk-order
//! independent via [`BTreeMap`].

pub mod completion;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use bamts_compiler::{
    diagnostic::Diagnostic,
    parser::parse,
    scanner::scan,
    source::{ScriptKind, SourceId, SourceText},
    syntax::{Token, TokenKind},
    telemetry::{PhaseTotals, TelemetryCollector},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::corpus::{
    ArtifactDirectory, CaseSpec, DigestAlgorithm, ExecutionMode, NODE_VERSION, OracleLimits,
    Provenance, bounded_output, cli_args, drain_stream, normalized_env, run_process,
};
use crate::oracle_pins::{
    COMPILER_COMMIT, COMPILER_DIGEST, COMPILER_REPOSITORY, COMPILER_TAG, COMPILER_URL,
    NPM_INTEGRITY, NPM_SPECIFIER, OraclePins, SUITE_COMMIT, SUITE_DIGEST, SUITE_REPOSITORY,
    SUITE_URL, verify_oracle_pins,
};
use crate::ts_ledger::{
    Backend, CompilerOracle, Confidence, Entry, EvidenceRecord, Facet, LEDGER_SCHEMA_VERSION,
    NpmOracle, Oracle, Partition, ReasonCode, SUITE_BASELINE_ROOT, SUITE_CASE_ROOT, Snapshot,
    Status, SuiteOracle, TimeoutClass, Totals, TsLedger, TsLedgerReader, TsLedgerWriter,
};
use crate::{ErrorCode, Result, VerificationError};

/// Pinned npm publication time used for every deterministic `generatedAt`.
pub const PINNED_GENERATED_AT: &str = "2026-07-08T15:55:18.431Z";
/// Per-stream capture cap (1 MiB). Exceeding yields [`FailureClass::OutputTruncated`].
pub const OUTPUT_CAP_BYTES: usize = 1 << 20;
/// Per-cell AOT scratch cap (1 GiB).
pub const AOT_SCRATCH_CAP_BYTES: u64 = 1 << 30;
/// Default snapshot root relative to the workspace.
pub const DEFAULT_SNAPSHOT_REL: &str = "verification/ts-suite";

const FRONTEND_DEFAULT_MS: u64 = 5_000;
const FRONTEND_CAP_MS: u64 = 30_000;
const EXECUTE_DEFAULT_MS: u64 = 10_000;
const EXECUTE_CAP_MS: u64 = 60_000;
const PROJECT_DEFAULT_MS: u64 = 30_000;
const PROJECT_CAP_MS: u64 = 60_000;
const WATCH_DEFAULT_MS: u64 = 30_000;
const WATCH_CAP_MS: u64 = 60_000;

/// All 15 ledger facets, in schema order.
pub const ALL_FACETS: [Facet; 15] = [
    Facet::Parse,
    Facet::Diagnostics,
    Facet::Types,
    Facet::Symbols,
    Facet::JsEmit,
    Facet::DtsEmit,
    Facet::ModuleResolution,
    Facet::Config,
    Facet::Cli,
    Facet::Build,
    Facet::Watch,
    Facet::NodeApi,
    Facet::LanguageService,
    Facet::Harness,
    Facet::Implementation,
];

/// Cell-level result taxonomy (not an [`ErrorCode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureClass {
    Pass,
    FailBehavior,
    FailDiagnostic,
    FailBaselineHash,
    Timeout,
    Crash,
    OutputTruncated,
    ProvenanceMismatch,
    LedgerIncomplete,
    SkipDeferred,
    SkipExcluded,
    HarnessError,
}

impl FailureClass {
    /// Stable display / rollup name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::FailBehavior => "FAIL_BEHAVIOR",
            Self::FailDiagnostic => "FAIL_DIAGNOSTIC",
            Self::FailBaselineHash => "FAIL_BASELINE_HASH",
            Self::Timeout => "TIMEOUT",
            Self::Crash => "CRASH",
            Self::OutputTruncated => "OUTPUT_TRUNCATED",
            Self::ProvenanceMismatch => "PROVENANCE_MISMATCH",
            Self::LedgerIncomplete => "LEDGER_INCOMPLETE",
            Self::SkipDeferred => "SKIP_DEFERRED",
            Self::SkipExcluded => "SKIP_EXCLUDED",
            Self::HarnessError => "HARNESS_ERROR",
        }
    }
}

/// Snapshot asset classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssetKind {
    CaseInput,
    BaselineFacet,
    DifferenceRecord,
    LicenseNotice,
}

impl AssetKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CaseInput => "caseInput",
            Self::BaselineFacet => "baselineFacet",
            Self::DifferenceRecord => "differenceRecord",
            Self::LicenseNotice => "licenseNotice",
        }
    }
}

/// Runner state machine stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RunState {
    PinVerify,
    Toolchains,
    Materialize,
    LedgerAudit,
    ShardPlan,
    ExecuteCell,
    Aggregate,
    Proof,
    Publish,
}

impl RunState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PinVerify => "PinVerify",
            Self::Toolchains => "Toolchains",
            Self::Materialize => "Materialize",
            Self::LedgerAudit => "LedgerAudit",
            Self::ShardPlan => "ShardPlan",
            Self::ExecuteCell => "ExecuteCell",
            Self::Aggregate => "Aggregate",
            Self::Proof => "Proof",
            Self::Publish => "Publish",
        }
    }
}

/// One executed (or skipped) cell outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellResult {
    pub entry_id: String,
    pub facet: Facet,
    pub backend: Backend,
    pub class: FailureClass,
    pub detail: String,
}

/// Sorted logical-path index of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiteIndex {
    pub entries: BTreeMap<String, IndexEntry>,
}

/// One index row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub logical_path: String,
    pub sha256: String,
    pub asset_kind: AssetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<Facet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<Partition>,
}

/// Options for [`sync_suite`].
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub verify_pin: bool,
    pub write_snapshot: bool,
    pub workspace_root: PathBuf,
    pub snapshot_root: PathBuf,
    /// When set, import from this extracted suite tree (hermetic; skips curl/tar).
    pub extracted_suite_root: Option<PathBuf>,
}

/// Status filter for run/ci.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Included,
    Deferred,
    Excluded,
    All,
}

/// Backend filter for run/ci.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFilter {
    One(Backend),
    All,
}

/// Filters applied during shard planning.
#[derive(Debug, Clone)]
pub struct RunFilterOptions {
    pub status: StatusFilter,
    pub slice: Option<String>,
    pub backends: BackendFilter,
    /// One-based `(k, N)` from `--shards k/N`.
    pub shards: Option<(u32, u32)>,
}

/// CI mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiMode {
    Pr,
    Nightly,
    WeeklyAudit,
}

/// Options for [`run_ci`].
#[derive(Debug, Clone)]
pub struct CiOptions {
    pub mode: CiMode,
    pub filters: RunFilterOptions,
    pub workspace_root: PathBuf,
    pub snapshot_root: PathBuf,
}

/// Aggregated suite run report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteRunReport {
    pub results: Vec<CellResult>,
    pub rollups: BTreeMap<FailureClass, usize>,
    pub state_reached: RunState,
}

/// Materialized snapshot handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteSnapshot {
    pub root: PathBuf,
    pub digest: String,
    pub index: SuiteIndex,
    pub ledger: TsLedger,
}

/// Snapshot that passed digest re-verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSuite {
    pub snapshot: SuiteSnapshot,
}

/// A materialized suite asset (type exported for `walk_extracted_tree`).
#[derive(Debug, Clone)]
pub struct ImportedAsset {
    pub logical_path: String,
    pub kind: AssetKind,
    pub digest_hex: String,
    pub bytes: Vec<u8>,
    pub facet: Option<Facet>,
    pub partition: Option<Partition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinDocument {
    npm_specifier: String,
    npm_integrity: String,
    compiler_repository: String,
    compiler_commit: String,
    compiler_tag: String,
    suite_repository: String,
    suite_commit: String,
}

impl PinDocument {
    fn from_pins(pins: &OraclePins) -> Self {
        Self {
            npm_specifier: pins.npm_specifier.clone(),
            npm_integrity: pins.npm_integrity.clone(),
            compiler_repository: pins.compiler_repository.clone(),
            compiler_commit: pins.compiler_commit.clone(),
            compiler_tag: pins.compiler_tag.clone(),
            suite_repository: pins.suite_repository.clone(),
            suite_commit: pins.suite_commit.clone(),
        }
    }

    fn expected() -> Self {
        Self::from_pins(&OraclePins::expected())
    }
}

/// Import / materialize a suite snapshot.
pub fn sync_suite(options: &SyncOptions) -> Result<SuiteSnapshot> {
    let pins = if options.verify_pin {
        if options.extracted_suite_root.is_some() {
            OraclePins::expected()
        } else {
            verify_oracle_pins(&options.workspace_root)?
        }
    } else {
        OraclePins::expected()
    };

    // Keep extraction temps alive until walks finish.
    let mut cleanup_suite: Option<TempDir> = None;
    let mut cleanup_compiler: Option<TempDir> = None;
    let assets = if let Some(root) = &options.extracted_suite_root {
        walk_extracted_tree(root)?
    } else {
        let archives = options.snapshot_root.join(".archives");
        fs::create_dir_all(&archives).map_err(|e| io_err(&archives, &e))?;
        let suite_archive = fetch_archive(SUITE_URL, SUITE_DIGEST, &archives)?;
        let compiler_archive = fetch_archive(COMPILER_URL, COMPILER_DIGEST, &archives)?;
        let suite_extracted = extract_archive(&suite_archive)?;
        let compiler_extracted = extract_archive(&compiler_archive)?;
        let suite_root = single_archive_root(suite_extracted.path())?;
        let compiler_root = single_archive_root(compiler_extracted.path())?;
        let mut assets = walk_extracted_tree(&suite_root)?;
        merge_compiler_license_notice(&mut assets, &compiler_root)?;
        cleanup_suite = Some(suite_extracted);
        cleanup_compiler = Some(compiler_extracted);
        assets
    };

    if !options.write_snapshot {
        let _ = (cleanup_suite, cleanup_compiler);
        return Err(VerificationError::new(
            ErrorCode::Usage,
            "sync requires --write-snapshot",
        ));
    }

    let snapshot = materialize_snapshot(&options.snapshot_root, &pins, &assets)?;
    let _ = (cleanup_suite, cleanup_compiler);
    verify_snapshot(&options.snapshot_root)?;
    Ok(snapshot)
}

/// Audit a ledger for completeness against discovered case inputs.
pub fn audit_ledger(
    ledger_path: &Path,
    require_complete: bool,
    discovered_inputs: &BTreeSet<String>,
) -> Result<TsLedger> {
    let ledger = TsLedgerReader::from_file(ledger_path)?;
    if !require_complete {
        return Ok(ledger);
    }

    let mut covered: BTreeSet<&str> = BTreeSet::new();
    for entry in &ledger.entries {
        covered.insert(entry.input.as_str());
    }
    for input in discovered_inputs {
        if !covered.contains(input.as_str()) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("LEDGER_INCOMPLETE: missing coverage for input `{input}`"),
            ));
        }
    }
    Ok(ledger)
}

/// Run the suite through the RunState machine with filters.
pub fn run_suite(
    workspace_root: &Path,
    snapshot_root: &Path,
    filters: &RunFilterOptions,
) -> Result<SuiteRunReport> {
    // PinVerify
    verify_pin_document(&snapshot_root.join("oracle/pin.json"))?;
    if filters_need_workspace_pins(filters) {
        // Live pin drift check is optional when snapshot pin already matches expected.
        let _ = workspace_root;
    }

    // Toolchains
    // Node gate only when execute backends are in scope for included cells.
    // Provisional ledger has no included rows; skip live Node for default runs.
    let _ = NODE_VERSION;

    // Materialize
    let verified = verify_snapshot(snapshot_root)?;

    // LedgerAudit
    let discovered = case_inputs_from_index(&verified.snapshot.index);
    let committed_ledger = workspace_root.join("verification/ts-suite-ledger.json");
    let ledger_path = if committed_ledger.is_file() {
        committed_ledger
    } else {
        snapshot_root.join("ledger.json")
    };
    let ledger = audit_ledger(&ledger_path, true, &discovered)?;
    let expected_digest = index_content_digest(&verified.snapshot.index);
    if ledger.snapshot.digest != expected_digest {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "classified ledger digest `{}` does not match suite index `{expected_digest}`",
                ledger.snapshot.digest
            ),
        ));
    }

    // ShardPlan
    let planned = plan_cells(&ledger, filters)?;

    // S2 check-cell context: the diagnostic code map and baseline groups are
    // loaded once per run (U2.8), and only when an included check cell is
    // planned for a facet that needs them — parse-only and skip-class runs
    // never touch them.
    let check_context = planned
        .iter()
        .any(|plan| {
            matches!(plan.entry.status, Status::Included)
                && matches!(plan.backend, Backend::Check)
                && matches!(
                    plan.entry.facet,
                    Facet::Diagnostics | Facet::Types | Facet::Symbols
                )
        })
        .then(|| {
            let code_map = crate::facets::load_diagnostic_code_map(workspace_root)?;
            let baseline_groups = crate::check_cells::baseline_groups(&verified.snapshot.index);
            Ok::<_, VerificationError>(crate::check_cells::CheckContext {
                code_map,
                baseline_groups,
            })
        })
        .transpose()?;

    // ExecuteCell
    let mut results = Vec::new();
    for plan in &planned {
        results.push(execute_planned_cell(
            snapshot_root,
            &verified.snapshot,
            check_context.as_ref(),
            plan,
        )?);
    }

    // Aggregate
    enforce_no_silent_skip(&planned, &results)?;

    // Proof
    let rollups = rollup_classes(&results);

    // Publish
    Ok(SuiteRunReport {
        results,
        rollups,
        state_reached: RunState::Publish,
    })
}

/// Run the suite and collect frontend phase telemetry from every executed cell.
///
/// A [`TelemetryCollector`] is active on the calling thread for the whole run,
/// so each check-backend compile the runner drives accumulates its
/// scan/parse/bind/check/emit/total wall into the returned [`PhaseTotals`]. The
/// runner executes cells synchronously on this thread, so the collector
/// observes every compile the backend performed.
pub fn run_suite_with_telemetry(
    workspace_root: &Path,
    snapshot_root: &Path,
    filters: &RunFilterOptions,
) -> Result<(SuiteRunReport, PhaseTotals)> {
    let collector = TelemetryCollector::start();
    let report = run_suite(workspace_root, snapshot_root, filters)?;
    let telemetry = collector.snapshot();
    Ok((report, telemetry))
}

/// CI entrypoint: same filters, mode retained for future policy differences.
pub fn run_ci(options: &CiOptions) -> Result<SuiteRunReport> {
    match options.mode {
        CiMode::Pr | CiMode::Nightly | CiMode::WeeklyAudit => run_suite(
            &options.workspace_root,
            &options.snapshot_root,
            &options.filters,
        ),
    }
}

fn filters_need_workspace_pins(_filters: &RunFilterOptions) -> bool {
    false
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedCell {
    pub(crate) entry: Entry,
    pub(crate) backend: Backend,
}

fn plan_cells(ledger: &TsLedger, filters: &RunFilterOptions) -> Result<Vec<PlannedCell>> {
    let mut planned = Vec::new();
    for entry in &ledger.entries {
        if !status_matches(filters.status, entry.status) {
            continue;
        }
        if let Some(slice) = &filters.slice {
            let needle = slice.to_ascii_lowercase();
            // Deferred/excluded rows name their gating slice in `blockedBy`;
            // included rows carry an empty `blockedBy`, so they are selected by
            // the slice that owns their facet's observation (§17.1/§17.2).
            let hit = if matches!(entry.status, Status::Included) {
                facet_owning_slice(entry.facet).eq_ignore_ascii_case(&needle)
            } else {
                entry
                    .blocked_by
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(&needle))
            };
            if !hit {
                continue;
            }
        }
        if let Some((k, n)) = filters.shards {
            let index = shard_index(&entry.shard_key, n)?;
            if index != k - 1 {
                continue;
            }
        }
        for backend in &entry.backends {
            if !backend_matches(filters.backends, *backend) {
                continue;
            }
            planned.push(PlannedCell {
                entry: entry.clone(),
                backend: *backend,
            });
        }
    }
    Ok(planned)
}

fn status_matches(filter: StatusFilter, status: Status) -> bool {
    match filter {
        StatusFilter::All => true,
        StatusFilter::Included => matches!(status, Status::Included),
        StatusFilter::Deferred => matches!(status, Status::Deferred),
        StatusFilter::Excluded => matches!(status, Status::Excluded),
    }
}

fn backend_matches(filter: BackendFilter, backend: Backend) -> bool {
    match filter {
        BackendFilter::All => true,
        BackendFilter::One(expected) => backend == expected,
    }
}

fn execute_planned_cell(
    snapshot_root: &Path,
    snapshot: &SuiteSnapshot,
    ctx: Option<&crate::check_cells::CheckContext>,
    plan: &PlannedCell,
) -> Result<CellResult> {
    match plan.entry.status {
        Status::Deferred => {
            return Ok(CellResult {
                entry_id: plan.entry.id.clone(),
                facet: plan.entry.facet,
                backend: plan.backend,
                class: FailureClass::SkipDeferred,
                detail: "deferred provisional row".to_owned(),
            });
        }
        Status::Excluded => {
            return Ok(CellResult {
                entry_id: plan.entry.id.clone(),
                facet: plan.entry.facet,
                backend: plan.backend,
                class: FailureClass::SkipExcluded,
                detail: "excluded provisional row".to_owned(),
            });
        }
        Status::Included => {}
    }

    match plan.backend {
        Backend::Check => execute_check_cell(snapshot_root, snapshot, ctx, plan),
        Backend::Interpreter | Backend::Jit | Backend::Aot => {
            execute_process_cell(snapshot_root, snapshot, plan)
        }
    }
}

fn execute_check_cell(
    _snapshot_root: &Path,
    snapshot: &SuiteSnapshot,
    ctx: Option<&crate::check_cells::CheckContext>,
    plan: &PlannedCell,
) -> Result<CellResult> {
    let Some(index_entry) = snapshot.index.entries.get(&plan.entry.input) else {
        return Ok(CellResult {
            entry_id: plan.entry.id.clone(),
            facet: plan.entry.facet,
            backend: plan.backend,
            class: FailureClass::LedgerIncomplete,
            detail: format!("missing index row for `{}`", plan.entry.input),
        });
    };
    match plan.entry.facet {
        Facet::Parse => execute_parse_check(snapshot, plan, index_entry),
        // U2.8 Phase A: the S2 `diagnostics` observation is wired.
        Facet::Diagnostics => {
            let Some(ctx) = ctx else {
                return Ok(CellResult {
                    entry_id: plan.entry.id.clone(),
                    facet: plan.entry.facet,
                    backend: plan.backend,
                    class: FailureClass::HarnessError,
                    detail: "diagnostics check context not loaded".to_owned(),
                });
            };
            crate::check_cells::execute_diagnostics_check(ctx, snapshot, plan, index_entry)
        }
        // U2.8 Phase B: the S2 `types` observation is wired.
        Facet::Types => {
            let Some(ctx) = ctx else {
                return Ok(CellResult {
                    entry_id: plan.entry.id.clone(),
                    facet: plan.entry.facet,
                    backend: plan.backend,
                    class: FailureClass::HarnessError,
                    detail: "types check context not loaded".to_owned(),
                });
            };
            crate::check_cells::execute_types_check(
                snapshot,
                &ctx.baseline_groups,
                plan,
                index_entry,
            )
        }
        // U2.8 Phase C: the S2 `symbols` observation is wired.
        Facet::Symbols => {
            let Some(ctx) = ctx else {
                return Ok(CellResult {
                    entry_id: plan.entry.id.clone(),
                    facet: plan.entry.facet,
                    backend: plan.backend,
                    class: FailureClass::HarnessError,
                    detail: "symbols check context not loaded".to_owned(),
                });
            };
            crate::check_cells::execute_symbols_check(
                snapshot,
                &ctx.baseline_groups,
                plan,
                index_entry,
            )
        }
        other => Ok(CellResult {
            entry_id: plan.entry.id.clone(),
            facet: plan.entry.facet,
            backend: plan.backend,
            class: FailureClass::HarnessError,
            detail: format!(
                "check backend has no observation wired for facet `{}`",
                other.as_str()
            ),
        }),
    }
}

/// Observe the S1 `parse` contract for one case, per program §6 ("scanner/parser
/// acceptance and recovery ... total recovery"): scan and parse the case source
/// with the script kind its logical path names, and pass iff the parse is
/// *total and well-formed* — the scanner and parser return without panicking,
/// their token streams tile the source exactly once, the parser preserves the
/// source identity, and every diagnostic is canonically ordered and anchored in
/// the source. A recovery that reports syntax errors (a negative test case)
/// still passes: the parse facet proves acceptance and recovery, not error
/// absence. Diagnostic *correspondence* is the `diagnostics` facet, owned by S2.
fn execute_parse_check(
    snapshot: &SuiteSnapshot,
    plan: &PlannedCell,
    index_entry: &IndexEntry,
) -> Result<CellResult> {
    let blob = snapshot.root.join("cases").join(&index_entry.sha256);
    let bytes = match fs::read(&blob) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(CellResult {
                entry_id: plan.entry.id.clone(),
                facet: plan.entry.facet,
                backend: plan.backend,
                class: FailureClass::HarnessError,
                detail: format!("cannot read case blob `{}`: {error}", blob.display()),
            });
        }
    };
    let text = decode_case_source(&bytes);

    let script_kind = script_kind_for(&index_entry.logical_path);
    // The scanner and parser are designed total, but the parse facet's whole
    // point is to prove that on the real suite; a panic is a genuine parse
    // failure, caught here so one case cannot abort the in-process run.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_facet_violation(&text, script_kind)
    }));
    let (class, detail) = match outcome {
        Ok(None) => (
            FailureClass::Pass,
            "scan+parse total; token stream tiles the source; diagnostics well-formed".to_owned(),
        ),
        Ok(Some(violation)) => (FailureClass::FailBehavior, violation),
        Err(_) => (
            FailureClass::Crash,
            "scanner or parser panicked on this input".to_owned(),
        ),
    };
    Ok(CellResult {
        entry_id: plan.entry.id.clone(),
        facet: plan.entry.facet,
        backend: plan.backend,
        class,
        detail,
    })
}

/// Runs the scanner and parser and returns the first structural parse-facet
/// violation, or `None` when the observation is total and well-formed.
fn parse_facet_violation(text: &str, script_kind: ScriptKind) -> Option<String> {
    // The parse facet observes the same frontend input bound as the compiler:
    // a case over the 16 MiB per-file budget is a violation, not a crash.
    let source = match SourceText::new(text) {
        Ok(source) => Arc::new(source),
        Err(error) => return Some(format!("frontend input bound rejected the case: {error}")),
    };
    let scanned = scan(SourceId::new(0), script_kind, Arc::clone(&source));
    if let Some(violation) = tiling_violation(
        "scanner",
        scanned.product().tokens(),
        scanned.product().eof(),
        &source,
        |token| scanned.product().token_text(token).map(str::to_owned),
    ) {
        return Some(violation);
    }

    let parsed = parse(scanned);
    let file = parsed.product();
    if file.source_id() != SourceId::new(0) {
        return Some("parser changed the source identity".to_owned());
    }
    if file.script_kind() != script_kind {
        return Some("parser changed the script kind".to_owned());
    }
    if file.source_text().as_str() != text {
        return Some("parser rewrote the source text".to_owned());
    }
    if let Some(violation) =
        tiling_violation("parser", file.tokens(), file.eof(), &source, |token| {
            file.token_text(token).map(str::to_owned)
        })
    {
        return Some(violation);
    }
    diagnostics_violation(parsed.diagnostics(), &source)
}

/// Verifies a token stream tiles its source exactly once, in order, with the
/// end-of-file token anchored at the source end and lexemes reproducing the
/// source byte for byte. Mirrors the corpus parser conformance invariants.
fn tiling_violation(
    label: &str,
    tokens: &[Token],
    eof: &Token,
    source: &SourceText,
    lexeme: impl Fn(&Token) -> Option<String>,
) -> Option<String> {
    let mut cursor = 0usize;
    let mut reconstructed = String::with_capacity(source.as_str().len());
    for (index, token) in tokens.iter().enumerate() {
        let range = token.range();
        if range.start().get() != cursor {
            return Some(format!(
                "{label}: token {index} ({:?}) starts at {} but the previous token ended at {cursor}",
                token.kind(),
                range.start().get()
            ));
        }
        if token.kind() == TokenKind::EndOfFile {
            return Some(format!(
                "{label}: token {index} is an end-of-file token inside the stream"
            ));
        }
        if token.is_missing() {
            if !range.is_empty() {
                return Some(format!(
                    "{label}: missing token {index} ({:?}) covers source text",
                    token.kind()
                ));
            }
        } else if range.is_empty() {
            return Some(format!(
                "{label}: token {index} ({:?}) makes no forward progress",
                token.kind()
            ));
        }
        let Some(lexeme_text) = lexeme(token) else {
            return Some(format!(
                "{label}: token {index} ({:?}) range {}..{} is not a slice of its source",
                token.kind(),
                range.start().get(),
                range.end().get()
            ));
        };
        if utf16_len(&lexeme_text) != range.len() {
            return Some(format!(
                "{label}: token {index} ({:?}) lexeme length disagrees with its range",
                token.kind()
            ));
        }
        reconstructed.push_str(&lexeme_text);
        cursor = range.end().get();
    }
    if eof.kind() != TokenKind::EndOfFile {
        return Some(format!(
            "{label}: terminal token is {:?}, not end-of-file",
            eof.kind()
        ));
    }
    if !eof.range().is_empty() {
        return Some(format!("{label}: the end-of-file token covers source text"));
    }
    if eof.range().start().get() != cursor {
        return Some(format!(
            "{label}: the end-of-file token is not anchored at the end of the last token"
        ));
    }
    if cursor != source.len_utf16().get() {
        return Some(format!(
            "{label}: the token stream stops before the end of the source"
        ));
    }
    if reconstructed != source.as_str() {
        return Some(format!(
            "{label}: concatenated lexemes do not reproduce the source"
        ));
    }
    None
}

/// Verifies diagnostics are canonically ordered and anchored inside the source.
fn diagnostics_violation(diagnostics: &[Diagnostic], source: &SourceText) -> Option<String> {
    if !diagnostics.is_sorted() {
        return Some("diagnostics are not in canonical order".to_owned());
    }
    for diagnostic in diagnostics {
        if diagnostic.source_id() != SourceId::new(0) {
            return Some(format!(
                "diagnostic {} is anchored in another source",
                diagnostic.code().as_str()
            ));
        }
        let range = diagnostic.range();
        if source.utf16_to_byte(range.start()).is_err()
            || source.utf16_to_byte(range.end()).is_err()
        {
            return Some(format!(
                "diagnostic {} is not anchored at source boundaries",
                diagnostic.code().as_str()
            ));
        }
    }
    None
}

/// UTF-16 code-unit length of a string, matching token range units.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Decodes case bytes to source text the way a TypeScript-compatible reader
/// does: a UTF-16 BE (`FE FF`) or LE (`FF FE`) byte-order mark selects UTF-16
/// decoding, a UTF-8 BOM (`EF BB BF`) is stripped, and the leading marker is
/// dropped. Malformed units decode lossily so the parser still observes a total
/// source — the parse facet proves recovery, not byte-perfect re-encoding.
pub(crate) fn decode_case_source(bytes: &[u8]) -> String {
    if let [0xFE, 0xFF, rest @ ..] = bytes {
        let units: Vec<u16> = rest
            .chunks(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair.get(1).copied().unwrap_or(0)]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let [0xFF, 0xFE, rest @ ..] = bytes {
        let units: Vec<u16> = rest
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair.get(1).copied().unwrap_or(0)]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(body).into_owned()
}

/// Maps a case logical path to the script kind the scanner and parser use,
/// mirroring the compiler's own extension rules (`.tsx`/`.jsx` enable JSX,
/// `.js`/`.mjs`/`.cjs` are JavaScript, everything else — `.ts`/`.mts`/`.cts`/
/// `.d.ts` and extensionless — is TypeScript).
pub(crate) fn script_kind_for(logical_path: &str) -> ScriptKind {
    let lower = logical_path.to_ascii_lowercase();
    if lower.ends_with(".tsx") {
        ScriptKind::TypeScriptReact
    } else if lower.ends_with(".jsx") {
        ScriptKind::JavaScriptReact
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
        ScriptKind::JavaScript
    } else {
        ScriptKind::TypeScript
    }
}

fn execute_process_cell(
    snapshot_root: &Path,
    snapshot: &SuiteSnapshot,
    plan: &PlannedCell,
) -> Result<CellResult> {
    let Some(index_entry) = snapshot.index.entries.get(&plan.entry.input) else {
        return Ok(CellResult {
            entry_id: plan.entry.id.clone(),
            facet: plan.entry.facet,
            backend: plan.backend,
            class: FailureClass::LedgerIncomplete,
            detail: format!("missing index row for `{}`", plan.entry.input),
        });
    };
    let case_blob = snapshot.root.join("cases").join(&index_entry.sha256);
    let (default_ms, _cap_ms) = budget_for(plan.entry.timeout_class);
    let limits = OracleLimits {
        timeout: Duration::from_millis(default_ms),
        max_output_bytes: OUTPUT_CAP_BYTES,
    };

    let mode = match plan.backend {
        Backend::Interpreter => ExecutionMode::Interpreter,
        Backend::Jit => ExecutionMode::Jit,
        Backend::Aot => ExecutionMode::Aot,
        Backend::Check => {
            return Err(VerificationError::new(
                ErrorCode::Usage,
                "internal: Check backend routed to process cell",
            ));
        }
    };

    if matches!(mode, ExecutionMode::Aot) {
        let spec = scratch_case_spec(&plan.entry.id);
        let artifacts = ArtifactDirectory::create(snapshot_root, &spec, mode)?;
        enforce_aot_scratch_cap(&artifacts.0)?;
        drop(artifacts);
        return Ok(CellResult {
            entry_id: plan.entry.id.clone(),
            facet: plan.entry.facet,
            backend: plan.backend,
            class: FailureClass::HarnessError,
            detail: "AOT cell requires full corpus CaseSpec wiring; scratch cleaned".to_owned(),
        });
    }

    // Keep corpus seams referenced so suite never invents a second process convention.
    let _ = (
        &case_blob,
        limits,
        NODE_VERSION,
        bounded_output as fn(Vec<u8>, usize) -> (Vec<u8>, bool),
        normalized_env as fn() -> Vec<(String, String)>,
        run_process,
        cli_args,
        drain_stream::<std::io::Empty>,
    );
    Ok(CellResult {
        entry_id: plan.entry.id.clone(),
        facet: plan.entry.facet,
        backend: plan.backend,
        class: FailureClass::HarnessError,
        detail: format!(
            "{} backend deferred to corpus driver wiring for included cells",
            backend_name(plan.backend)
        ),
    })
}

fn scratch_case_spec(id: &str) -> CaseSpec {
    CaseSpec {
        id: id.replace(['/', '#'], "_"),
        provenance: Provenance::LocalContent {
            digest_algorithm: DigestAlgorithm::Sha256,
            digest: "0".repeat(64),
        },
        license: "Apache-2.0".to_owned(),
        source_dir: "src".to_owned(),
        entrypoint: "main.ts".to_owned(),
        node_args: Vec::new(),
        expected_timeout_ms: EXECUTE_DEFAULT_MS,
        constructs: Vec::new(),
        source_files: vec!["main.ts".to_owned()],
        compiler_args: Vec::new(),
    }
}

fn enforce_no_silent_skip(planned: &[PlannedCell], results: &[CellResult]) -> Result<()> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for result in results {
        seen.insert((
            result.entry_id.clone(),
            backend_name(result.backend).to_owned(),
        ));
    }
    for plan in planned {
        if !matches!(plan.entry.status, Status::Included) {
            continue;
        }
        let key = (plan.entry.id.clone(), backend_name(plan.backend).to_owned());
        if !seen.contains(&key) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "LEDGER_INCOMPLETE: NO_SILENT_SKIP missing result for `{}` backend `{}`",
                    plan.entry.id,
                    backend_name(plan.backend)
                ),
            ));
        }
    }
    Ok(())
}

fn rollup_classes(results: &[CellResult]) -> BTreeMap<FailureClass, usize> {
    let mut rollups = BTreeMap::new();
    for result in results {
        *rollups.entry(result.class).or_insert(0) += 1;
    }
    rollups
}

/// Default and cap budgets (milliseconds) for a timeout class.
pub fn budget_for(class: TimeoutClass) -> (u64, u64) {
    match class {
        TimeoutClass::Frontend => (FRONTEND_DEFAULT_MS, FRONTEND_CAP_MS),
        TimeoutClass::Execute => (EXECUTE_DEFAULT_MS, EXECUTE_CAP_MS),
        TimeoutClass::Project => (PROJECT_DEFAULT_MS, PROJECT_CAP_MS),
        TimeoutClass::Watch => (WATCH_DEFAULT_MS, WATCH_CAP_MS),
    }
}

/// Map truncation flags onto [`FailureClass::OutputTruncated`].
pub fn classify_truncation(stdout_truncated: bool, stderr_truncated: bool) -> Option<FailureClass> {
    if stdout_truncated || stderr_truncated {
        Some(FailureClass::OutputTruncated)
    } else {
        None
    }
}

/// Reject AOT scratch directories whose recursive size exceeds the cap.
pub fn enforce_aot_scratch_cap(path: &Path) -> Result<()> {
    let size = directory_size(path)?;
    enforce_aot_scratch_size(path, size)
}

/// Size-gated helper used by tests to avoid writing a full gibibyte.
pub fn enforce_aot_scratch_size(path: &Path, size: u64) -> Result<()> {
    if size > AOT_SCRATCH_CAP_BYTES {
        let _ = fs::remove_dir_all(path);
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "AOT scratch at `{}` is {size} bytes, exceeds cap {AOT_SCRATCH_CAP_BYTES}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| io_err(&current, &e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&current, &e))?;
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| io_err(&entry.path(), &e))?;
            // Check the link itself before following: `entry.metadata()` would
            // resolve the target, making `is_symlink()` always false and the
            // guard below unreachable — letting the walk escape the scratch dir.
            if meta.file_type().is_symlink() {
                return Err(VerificationError::new(
                    ErrorCode::ProvenanceMismatch,
                    format!("symlink rejected in scratch `{}`", entry.path().display()),
                ));
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}
type PathClassification = Option<(AssetKind, Option<Partition>, Option<Facet>)>;

/// Classify a suite-relative logical path.
pub fn classify_logical_path(logical_path: &str) -> Result<PathClassification> {
    if logical_path.is_empty() {
        return Ok(None);
    }
    let path = Path::new(logical_path);
    if path.is_absolute() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("absolute path rejected: `{logical_path}`"),
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(VerificationError::new(
                    ErrorCode::Schema,
                    format!("path traversal rejected: `{logical_path}`"),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(VerificationError::new(
                    ErrorCode::Schema,
                    format!("absolute path rejected: `{logical_path}`"),
                ));
            }
        }
    }

    let lower = logical_path.to_ascii_lowercase();
    if lower.contains("hereby")
        || lower.contains("internal/testutil")
        || lower.contains("/runners/")
        || lower.contains("\\runners\\")
    {
        return Ok(None);
    }

    if let Some(rest) = logical_path.strip_prefix("tests/cases/") {
        let partition = match rest.split('/').next().unwrap_or("") {
            "compiler" => Partition::Compiler,
            "conformance" => Partition::Conformance,
            "project" => Partition::Project,
            "projects" => Partition::Projects,
            "transpile" => Partition::Transpile,
            "unittests" => Partition::UnitTests,
            _ => return Ok(None),
        };
        if rest.contains('/') {
            return Ok(Some((AssetKind::CaseInput, Some(partition), None)));
        }
        return Ok(None);
    }

    if logical_path.starts_with("tests/baselines/reference/")
        || logical_path.starts_with("testdata/baselines/reference/")
    {
        let facet = baseline_facet_from_path(logical_path);
        return Ok(Some((AssetKind::BaselineFacet, None, facet)));
    }

    if logical_path == "testdata/submoduleAccepted.txt"
        || logical_path == "testdata/submoduleTriaged.txt"
    {
        return Ok(Some((AssetKind::DifferenceRecord, None, None)));
    }

    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let base_upper = basename.to_ascii_uppercase();
    if base_upper.starts_with("LICENSE") || base_upper.starts_with("NOTICE") {
        return Ok(Some((AssetKind::LicenseNotice, None, None)));
    }

    Ok(None)
}

fn baseline_facet_from_path(path: &str) -> Option<Facet> {
    if path.ends_with(".errors.txt") {
        Some(Facet::Diagnostics)
    } else if path.ends_with(".types") {
        Some(Facet::Types)
    } else if path.ends_with(".symbols") {
        Some(Facet::Symbols)
    } else if path.ends_with(".d.ts") {
        Some(Facet::DtsEmit)
    } else if path.ends_with(".js") {
        Some(Facet::JsEmit)
    } else {
        None
    }
}

/// Walk an extracted suite tree into a deterministic asset map.
pub fn walk_extracted_tree(root: &Path) -> Result<BTreeMap<String, ImportedAsset>> {
    let mut assets = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| io_err(&current, &e))?;
        let mut children = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&current, &e))?;
            children.push(entry.path());
        }
        children.sort();
        for child in children {
            let meta = fs::symlink_metadata(&child).map_err(|e| io_err(&child, &e))?;
            let rel = child
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if meta.file_type().is_symlink() {
                if member_looks_allowlisted(&rel) {
                    return Err(VerificationError::new(
                        ErrorCode::ProvenanceMismatch,
                        format!("symlink rejected: `{}`", child.display()),
                    ));
                }
                continue;
            }
            if meta.is_dir() {
                stack.push(child);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Some((kind, partition, facet)) = classify_logical_path(&rel)? else {
                continue;
            };
            let bytes = fs::read(&child).map_err(|e| io_err(&child, &e))?;
            let digest_hex = sha256_hex(&bytes);
            if assets.contains_key(&rel) {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("duplicate logical path `{rel}`"),
                ));
            }
            assets.insert(
                rel.clone(),
                ImportedAsset {
                    logical_path: rel,
                    kind,
                    digest_hex,
                    bytes,
                    facet,
                    partition,
                },
            );
        }
    }
    Ok(assets)
}

/// Import LICENSE/NOTICE assets from the pinned compiler archive under a `compiler/` prefix.
fn merge_compiler_license_notice(
    assets: &mut BTreeMap<String, ImportedAsset>,
    compiler_root: &Path,
) -> Result<()> {
    reject_extracted_symlinks(compiler_root)?;
    let mut stack = vec![compiler_root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| io_err(&current, &e))?;
        let mut children = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&current, &e))?;
            children.push(entry.path());
        }
        children.sort();
        for child in children {
            let meta = fs::symlink_metadata(&child).map_err(|e| io_err(&child, &e))?;
            if meta.file_type().is_symlink() {
                let basename = child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if basename.starts_with("LICENSE") || basename.starts_with("NOTICE") {
                    return Err(VerificationError::new(
                        ErrorCode::ProvenanceMismatch,
                        format!("symlink rejected: `{}`", child.display()),
                    ));
                }
                continue;
            }
            if meta.is_dir() {
                stack.push(child);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let basename = child
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_ascii_uppercase();
            if !(basename.starts_with("LICENSE") || basename.starts_with("NOTICE")) {
                continue;
            }
            let rel = child
                .strip_prefix(compiler_root)
                .map_err(|_| {
                    VerificationError::new(
                        ErrorCode::Schema,
                        format!("path escapes compiler root: `{}`", child.display()),
                    )
                })?
                .to_string_lossy()
                .replace('\\', "/");
            let logical = format!("compiler/{rel}");
            let Some((kind, partition, facet)) = classify_logical_path(&logical)? else {
                continue;
            };
            if !matches!(kind, AssetKind::LicenseNotice) {
                continue;
            }
            let bytes = fs::read(&child).map_err(|e| io_err(&child, &e))?;
            let digest_hex = sha256_hex(&bytes);
            if assets.contains_key(&logical) {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("duplicate logical path `{logical}`"),
                ));
            }
            assets.insert(
                logical.clone(),
                ImportedAsset {
                    logical_path: logical,
                    kind,
                    digest_hex,
                    bytes,
                    facet,
                    partition,
                },
            );
        }
    }
    Ok(())
}

fn materialize_snapshot(
    snapshot_root: &Path,
    pins: &OraclePins,
    assets: &BTreeMap<String, ImportedAsset>,
) -> Result<SuiteSnapshot> {
    let cases_dir = snapshot_root.join("cases");
    let baselines_dir = snapshot_root.join("baselines");
    let notice_dir = snapshot_root.join("oracle/NOTICE");
    fs::create_dir_all(&cases_dir).map_err(|e| io_err(&cases_dir, &e))?;
    fs::create_dir_all(&baselines_dir).map_err(|e| io_err(&baselines_dir, &e))?;
    fs::create_dir_all(&notice_dir).map_err(|e| io_err(&notice_dir, &e))?;

    let mut index = SuiteIndex {
        entries: BTreeMap::new(),
    };

    for asset in assets.values() {
        match asset.kind {
            AssetKind::CaseInput => {
                store_blob(&cases_dir, &asset.digest_hex, &asset.bytes)?;
            }
            AssetKind::BaselineFacet => {
                store_blob(&baselines_dir, &asset.digest_hex, &asset.bytes)?;
            }
            AssetKind::DifferenceRecord => {
                store_blob(&baselines_dir, &asset.digest_hex, &asset.bytes)?;
            }
            AssetKind::LicenseNotice => {
                let dest = notice_dir.join(asset.logical_path.replace('/', "__"));
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
                }
                write_then_verify(&dest, &asset.bytes)?;
            }
        }
        index.entries.insert(
            asset.logical_path.clone(),
            IndexEntry {
                logical_path: asset.logical_path.clone(),
                sha256: asset.digest_hex.clone(),
                asset_kind: asset.kind,
                facet: asset.facet,
                partition: asset.partition,
            },
        );
    }

    let ledger = build_provisional_ledger(pins, &index)?;
    let pin_bytes = canonicalize_json_bytes(&PinDocument::from_pins(pins))?;
    let index_bytes = canonicalize_json_bytes(&index)?;
    let ledger_bytes = TsLedgerWriter::to_vec(&ledger)?;

    let oracle_dir = snapshot_root.join("oracle");
    fs::create_dir_all(&oracle_dir).map_err(|e| io_err(&oracle_dir, &e))?;
    write_then_verify(&oracle_dir.join("pin.json"), &pin_bytes)?;
    write_then_verify(&snapshot_root.join("index.json"), &index_bytes)?;
    write_then_verify(&snapshot_root.join("ledger.json"), &ledger_bytes)?;

    let digest = sha256_hex_concat(&[&pin_bytes, &index_bytes, &ledger_bytes]);
    write_then_verify(
        &snapshot_root.join("snapshot.sha256"),
        format!("{digest}\n").as_bytes(),
    )?;

    Ok(SuiteSnapshot {
        root: snapshot_root.to_path_buf(),
        digest,
        index,
        ledger,
    })
}

/// Build the provisional U0.4 ledger covering every case input × all 15 facets.
pub fn build_provisional_ledger(pins: &OraclePins, index: &SuiteIndex) -> Result<TsLedger> {
    let mut entries = Vec::new();
    for entry in index.entries.values() {
        if !matches!(entry.asset_kind, AssetKind::CaseInput) {
            continue;
        }
        let partition = entry.partition.ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!("case input `{}` missing partition", entry.logical_path),
            )
        })?;
        for facet in ALL_FACETS {
            entries.push(provisional_entry(&entry.logical_path, partition, facet));
        }
    }

    let content_digest = index_content_digest(index);
    let mut ledger = TsLedger {
        schema_version: LEDGER_SCHEMA_VERSION,
        oracle: Oracle {
            npm: NpmOracle {
                specifier: pins.npm_specifier.clone(),
                integrity: pins.npm_integrity.clone(),
                published_at: PINNED_GENERATED_AT.to_owned(),
            },
            compiler: CompilerOracle {
                repository: pins.compiler_repository.clone(),
                commit: pins.compiler_commit.clone(),
                tag: pins.compiler_tag.clone(),
            },
            suite: SuiteOracle {
                repository: pins.suite_repository.clone(),
                commit: pins.suite_commit.clone(),
                case_root: SUITE_CASE_ROOT.to_owned(),
                baseline_root: SUITE_BASELINE_ROOT.to_owned(),
            },
        },
        snapshot: Snapshot {
            digest: content_digest,
            generated_at: PINNED_GENERATED_AT.to_owned(),
            entry_count: 0,
            input_count: 0,
        },
        entries,
        totals: Totals {
            discovered_inputs: 0,
            entries: 0,
            included: 0,
            deferred: 0,
            excluded: 0,
        },
    };
    ledger.sort_and_recompute();
    ledger.totals.discovered_inputs = ledger.snapshot.input_count;
    Ok(ledger)
}

fn provisional_entry(input: &str, partition: Partition, facet: Facet) -> Entry {
    let (status, reason_code, blocked_by) = match facet {
        Facet::Harness => (
            Status::Excluded,
            ReasonCode::UpstreamHarnessOnly,
            Vec::new(),
        ),
        Facet::Implementation => (
            Status::Excluded,
            ReasonCode::UpstreamImplementationOnly,
            Vec::new(),
        ),
        Facet::Parse
        | Facet::Diagnostics
        | Facet::Types
        | Facet::Symbols
        | Facet::JsEmit
        | Facet::DtsEmit
        | Facet::ModuleResolution
        | Facet::Config
        | Facet::Cli
        | Facet::Build
        | Facet::Watch
        | Facet::NodeApi
        | Facet::LanguageService => (
            Status::Deferred,
            ReasonCode::PromisedNotImplemented,
            vec!["S0".to_owned()],
        ),
    };

    let backends = match facet {
        Facet::JsEmit | Facet::NodeApi => {
            vec![Backend::Interpreter, Backend::Jit, Backend::Aot]
        }
        Facet::Parse
        | Facet::Diagnostics
        | Facet::Types
        | Facet::Symbols
        | Facet::DtsEmit
        | Facet::ModuleResolution
        | Facet::Config
        | Facet::Cli
        | Facet::Build
        | Facet::Watch
        | Facet::LanguageService
        | Facet::Harness
        | Facet::Implementation => vec![Backend::Check],
    };

    let timeout_class = match facet {
        Facet::Watch => TimeoutClass::Watch,
        Facet::Build => TimeoutClass::Project,
        Facet::JsEmit | Facet::NodeApi => TimeoutClass::Execute,
        Facet::Parse
        | Facet::Diagnostics
        | Facet::Types
        | Facet::Symbols
        | Facet::DtsEmit
        | Facet::ModuleResolution
        | Facet::Config
        | Facet::Cli
        | Facet::LanguageService
        | Facet::Harness
        | Facet::Implementation => TimeoutClass::Frontend,
    };

    let id = format!("{input}#{}", facet.as_str());
    let shard_key = compute_shard_key(&id, facet, &backends);

    Entry {
        id,
        input: input.to_owned(),
        partition,
        facet,
        status,
        surface: format!("{}.{}", partition.as_str(), facet.as_str()),
        reason_code,
        backends,
        timeout_class,
        shard_key,
        expected: Vec::new(),
        blocked_by,
        evidence: vec![EvidenceRecord {
            tier: 1,
            confidence: Confidence::Verified,
            url: SUITE_URL.to_owned(),
            note: Some("provisional U0.4 tier-1 evidence".to_owned()),
        }],
    }
}

/// Classify a single (`partition`, `facet`) cell for the U0.5 ledger v1 per
/// `.outline/wayfinder/type-script-7-compatibility-program.md` §17.2 and §2.1.
///
/// Returns the `(status, reasonCode, blockedBy)` triple. Facet-level exclusions
/// (`harness`/`implementation`) are never owned by any slice and win over every
/// partition rule; whole-partition dispositions (Node API / language-service /
/// watch families and upstream unit tests) win over the per-facet owning-slice
/// fallback used by the general compiler/conformance/project partitions.
pub fn classify_cell(partition: Partition, facet: Facet) -> (Status, ReasonCode, Vec<String>) {
    // Facet-level exclusions: never owned (§17.2 `harness` / `implementation`).
    match facet {
        Facet::Harness => {
            return (
                Status::Excluded,
                ReasonCode::UpstreamHarnessOnly,
                Vec::new(),
            );
        }
        Facet::Implementation => {
            return (
                Status::Excluded,
                ReasonCode::UpstreamImplementationOnly,
                Vec::new(),
            );
        }
        _ => {}
    }

    // Whole-partition dispositions (§17.2 rows keyed by partition).
    match partition {
        // Upstream compiler-internal unit tests assert no promised external
        // contract, so they are excluded with evidence (§17.2 `unittests`).
        Partition::UnitTests => (
            Status::Excluded,
            ReasonCode::UpstreamImplementationOnly,
            Vec::new(),
        ),
        // Node API family (`api`/`transpile`/`astnav`) → deferred until S10.
        Partition::Transpile | Partition::Api | Partition::AstNav => deferred_to("S10"),
        // Language service / LSP (`fourslash`/`lsp`) → deferred until S11.
        Partition::Fourslash | Partition::Lsp => deferred_to("S11"),
        // Watch partitions (`tscWatch`/`tsbuildWatch`) → deferred until S9.
        Partition::TscWatch | Partition::TsBuildWatch => deferred_to("S9"),
        // Configuration / CLI partitions: the entire partition is owned by S7
        // per §17.2, regardless of the specific facet.
        Partition::Config | Partition::Tsc | Partition::TsOptions => deferred_to("S7"),
        // Build-mode partition: the entire partition is owned by S8 per §17.2.
        Partition::TsBuild => deferred_to("S8"),
        // General partitions (compiler/conformance/project/projects/other):
        // facet-partitioned rows; the facet names the owning slice. U1.3 flips
        // the `parse` facet to an included compiler contract (S1 owns parsing);
        // U2.8 flips `diagnostics` the same way (S2 owns diagnostic
        // correspondence) — with per-input refinement in [`classify_s2_entry`]
        // (project diagnostics re-defer to S5; projects rows are excluded).
        // `types` and `symbols` are refined per-input in [`classify_s2_entry`]
        // (Phase B/C baseline ownership: included iff the input uniquely owns
        // the facet's baseline, else excluded upstream-harness).
        Partition::Compiler
        | Partition::Conformance
        | Partition::Project
        | Partition::Projects
        | Partition::Other => {
            if matches!(facet, Facet::Parse | Facet::Diagnostics) {
                (
                    Status::Included,
                    ReasonCode::PromisedCompilerContract,
                    Vec::new(),
                )
            } else {
                deferred_to(facet_owning_slice(facet))
            }
        }
    }
}

/// U2.8 per-input refinement of the S2 facet classification.
///
/// `classify_cell` is pure in `(partition, facet)`; baseline-aware and
/// project rules need the input path, so this runs inside
/// [`build_classified_ledger`]. Returns `None` when the generic rule stands.
fn classify_s2_entry(
    input: &str,
    partition: Partition,
    facet: Facet,
    s2: &crate::check_cells::S2Classification,
) -> Option<(Status, ReasonCode, Vec<String>)> {
    match facet {
        Facet::Diagnostics => match partition {
            // Project diagnostics baselines (`reference/project/<name>/…`) are
            // module-resolution/tsconfig observations (TS2792/TS5107); §17.2
            // assigns resolution semantics to S5.
            Partition::Project => Some(deferred_to("S5")),
            // The `projects` partition carries no upstream baseline artifacts.
            Partition::Projects => Some((
                Status::Excluded,
                ReasonCode::UpstreamHarnessOnly,
                Vec::new(),
            )),
            _ => None,
        },
        // Phase B: `types` is an included compiler contract for a general
        // partition input that uniquely owns a `.types` baseline. Every other
        // input is excluded upstream-harness-only: project/projects (no upstream
        // `.types`), `APISample_*` / `@noTypesAndSymbols` (no baseline), and the
        // ambiguous side of a duplicate stem (index alone cannot name the owner).
        Facet::Types => match partition {
            Partition::Project | Partition::Projects => Some((
                Status::Excluded,
                ReasonCode::UpstreamHarnessOnly,
                Vec::new(),
            )),
            Partition::Compiler | Partition::Conformance | Partition::Other => {
                if s2.owns_types_baseline(input) {
                    Some((
                        Status::Included,
                        ReasonCode::PromisedCompilerContract,
                        Vec::new(),
                    ))
                } else {
                    Some((
                        Status::Excluded,
                        ReasonCode::UpstreamHarnessOnly,
                        Vec::new(),
                    ))
                }
            }
            _ => None,
        },
        // Phase C: `symbols` follows the identical ownership rule as `types`
        // (a general-partition input included iff it uniquely owns a `.symbols`
        // baseline; project/projects, `@noTypesAndSymbols`, `APISample_*`, and
        // the ambiguous side of a duplicate stem are excluded upstream-harness).
        Facet::Symbols => match partition {
            Partition::Project | Partition::Projects => Some((
                Status::Excluded,
                ReasonCode::UpstreamHarnessOnly,
                Vec::new(),
            )),
            Partition::Compiler | Partition::Conformance | Partition::Other => {
                if s2.owns_symbols_baseline(input) {
                    Some((
                        Status::Included,
                        ReasonCode::PromisedCompilerContract,
                        Vec::new(),
                    ))
                } else {
                    Some((
                        Status::Excluded,
                        ReasonCode::UpstreamHarnessOnly,
                        Vec::new(),
                    ))
                }
            }
            _ => None,
        },
        _ => None,
    }
}

fn deferred_to(slice: &str) -> (Status, ReasonCode, Vec<String>) {
    (
        Status::Deferred,
        ReasonCode::PromisedNotImplemented,
        vec![slice.to_owned()],
    )
}

/// The vertical slice that owns a facet's observation (§17.1/§17.2 ownership).
fn facet_owning_slice(facet: Facet) -> &'static str {
    match facet {
        Facet::Parse => "S1",
        Facet::Diagnostics | Facet::Types | Facet::Symbols => "S2",
        Facet::JsEmit | Facet::DtsEmit => "S4",
        Facet::ModuleResolution => "S5",
        Facet::Config | Facet::Cli => "S7",
        Facet::Build => "S8",
        Facet::Watch => "S9",
        Facet::NodeApi => "S10",
        Facet::LanguageService => "S11",
        // Excluded before reaching here; kept exhaustive for the compiler.
        Facet::Harness | Facet::Implementation => "S0",
    }
}

/// Build the U0.5 classified ledger (v1): every discovered case input × all 15
/// facets, classified per [`classify_cell`]. Reuses the U0.4 provisional layout
/// for entry shape, shard keys, backends, and timeout classes; `snapshot.digest`
/// is the index content digest, while the snapshot's top-level `snapshot.sha256`
/// is computed separately over the whole manifest (pin + index + ledger).
pub fn build_classified_ledger(pins: &OraclePins, index: &SuiteIndex) -> Result<TsLedger> {
    let mut ledger = build_provisional_ledger(pins, index)?;
    ledger.snapshot.digest = index_content_digest(index);
    let s2 = crate::check_cells::S2Classification::from_index(index);
    for entry in &mut ledger.entries {
        let (status, reason_code, blocked_by) =
            classify_s2_entry(&entry.input, entry.partition, entry.facet, &s2)
                .unwrap_or_else(|| classify_cell(entry.partition, entry.facet));
        entry.status = status;
        entry.reason_code = reason_code;
        entry.blocked_by = blocked_by;
        entry.evidence = vec![EvidenceRecord {
            tier: 1,
            confidence: Confidence::Verified,
            url: SUITE_URL.to_owned(),
            note: Some(classification_note(entry.partition, entry.facet, status)),
        }];
    }
    ledger.sort_and_recompute();
    ledger.totals.discovered_inputs = ledger.snapshot.input_count;
    Ok(ledger)
}

fn classification_note(partition: Partition, facet: Facet, status: Status) -> String {
    let disposition = match status {
        Status::Included => "included",
        Status::Deferred => "deferred",
        Status::Excluded => "excluded",
    };
    format!(
        "U0.5 §17.2 classification (U1.3 parse, U2.8 diagnostics): partition `{}`, facet `{}` → {disposition}",
        partition.as_str(),
        facet.as_str(),
    )
}

/// Classify a materialized, provenance-verified snapshot and write ledger v1 to
/// `ledger_out`. Also overwrites the snapshot's own `ledger.json` and
/// recomputes `snapshot.sha256` so the runner sees the classified ledger.
pub fn write_suite_ledger(snapshot_root: &Path, ledger_out: &Path) -> Result<TsLedger> {
    let verified = verify_snapshot(snapshot_root)?;
    let pins = OraclePins::expected();
    let ledger = build_classified_ledger(&pins, &verified.snapshot.index)?;
    let ledger_bytes = TsLedgerWriter::to_vec(&ledger)?;

    if let Some(parent) = ledger_out.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
    }
    write_then_verify(ledger_out, &ledger_bytes)?;
    write_then_verify(&snapshot_root.join("ledger.json"), &ledger_bytes)?;

    let pin_bytes = fs::read(snapshot_root.join("oracle/pin.json"))
        .map_err(|e| io_err(&snapshot_root.join("oracle/pin.json"), &e))?;
    let index_bytes = fs::read(snapshot_root.join("index.json"))
        .map_err(|e| io_err(&snapshot_root.join("index.json"), &e))?;
    let digest = sha256_hex_concat(&[&pin_bytes, &index_bytes, &ledger_bytes]);
    write_then_verify(
        &snapshot_root.join("snapshot.sha256"),
        format!("{digest}\n").as_bytes(),
    )?;
    Ok(ledger)
}

fn index_content_digest(index: &SuiteIndex) -> String {
    let mut hasher = Sha256::new();
    for (path, entry) in &index.entries {
        hasher.update(path.as_bytes());
        hasher.update(b"|");
        hasher.update(entry.sha256.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(hasher.finalize())
}

/// Compute the exact shard key formula.
pub fn compute_shard_key(id: &str, facet: Facet, backends: &[Backend]) -> String {
    let mut names: Vec<&str> = backends.iter().copied().map(backend_name).collect();
    names.sort_unstable();
    let joined = names.join(",");
    let material = format!("{id}|{}|{joined}", facet.as_str());
    let digest = sha256_hex(material.as_bytes());
    digest[..16].to_owned()
}

/// Parse the first 8 hex chars of a shard key as `u32` and reduce mod `n`.
pub fn shard_index(shard_key: &str, n: u32) -> Result<u32> {
    if n == 0 {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            "shard count N must be > 0",
        ));
    }
    if shard_key.len() < 8 {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            format!("shard key `{shard_key}` is too short"),
        ));
    }
    let prefix = u32::from_str_radix(&shard_key[..8], 16).map_err(|error| {
        VerificationError::new(
            ErrorCode::Usage,
            format!("invalid shard key prefix `{shard_key}`: {error}"),
        )
    })?;
    Ok(prefix % n)
}

/// Parse `--shards k/N` (1-based).
pub fn parse_shards(value: &str) -> Result<(u32, u32)> {
    let Some((k_text, n_text)) = value.split_once('/') else {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            format!("invalid --shards `{value}`; expected k/N"),
        ));
    };
    let k: u32 = k_text.parse().map_err(|_| {
        VerificationError::new(
            ErrorCode::Usage,
            format!("invalid --shards `{value}`; k must be an integer"),
        )
    })?;
    let n: u32 = n_text.parse().map_err(|_| {
        VerificationError::new(
            ErrorCode::Usage,
            format!("invalid --shards `{value}`; N must be an integer"),
        )
    })?;
    if n == 0 || k == 0 || k > n {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            format!("invalid --shards `{value}`; require 1 <= k <= N and N > 0"),
        ));
    }
    Ok((k, n))
}

pub fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Check => "check",
        Backend::Interpreter => "interpreter",
        Backend::Jit => "jit",
        Backend::Aot => "aot",
    }
}

#[allow(dead_code)]
fn status_name(status: Status) -> &'static str {
    match status {
        Status::Included => "included",
        Status::Deferred => "deferred",
        Status::Excluded => "excluded",
    }
}

/// Re-verify blob digests and `snapshot.sha256`.
pub fn verify_snapshot(snapshot_root: &Path) -> Result<VerifiedSuite> {
    let pin_bytes = fs::read(snapshot_root.join("oracle/pin.json"))
        .map_err(|e| io_err(&snapshot_root.join("oracle/pin.json"), &e))?;
    let index_bytes = fs::read(snapshot_root.join("index.json"))
        .map_err(|e| io_err(&snapshot_root.join("index.json"), &e))?;
    let ledger_bytes = fs::read(snapshot_root.join("ledger.json"))
        .map_err(|e| io_err(&snapshot_root.join("ledger.json"), &e))?;
    let expected = sha256_hex_concat(&[&pin_bytes, &index_bytes, &ledger_bytes]);
    let recorded = fs::read_to_string(snapshot_root.join("snapshot.sha256"))
        .map_err(|e| io_err(&snapshot_root.join("snapshot.sha256"), &e))?;
    let recorded = recorded.trim();
    if recorded != expected {
        return Err(VerificationError::new(
            ErrorCode::ProvenanceMismatch,
            format!("snapshot.sha256 mismatch: recorded `{recorded}`, computed `{expected}`"),
        ));
    }

    let index: SuiteIndex = serde_json::from_slice(&index_bytes)
        .map_err(|e| VerificationError::new(ErrorCode::Json, format!("index.json: {e}")))?;
    for entry in index.entries.values() {
        let dir = match entry.asset_kind {
            AssetKind::CaseInput => snapshot_root.join("cases"),
            AssetKind::BaselineFacet | AssetKind::DifferenceRecord => {
                snapshot_root.join("baselines")
            }
            AssetKind::LicenseNotice => continue,
        };
        let path = dir.join(&entry.sha256);
        let bytes = fs::read(&path).map_err(|e| io_err(&path, &e))?;
        let actual = sha256_hex(&bytes);
        if actual != entry.sha256 {
            return Err(VerificationError::new(
                ErrorCode::Digest,
                format!(
                    "blob digest mismatch for `{}`: index `{}`, actual `{actual}`",
                    entry.logical_path, entry.sha256
                ),
            ));
        }
    }

    verify_pin_document_bytes(&pin_bytes)?;
    let ledger = TsLedgerReader::from_slice(&ledger_bytes)?;

    Ok(VerifiedSuite {
        snapshot: SuiteSnapshot {
            root: snapshot_root.to_path_buf(),
            digest: expected,
            index,
            ledger,
        },
    })
}

fn verify_pin_document(path: &Path) -> Result<()> {
    let bytes = fs::read(path).map_err(|e| io_err(path, &e))?;
    verify_pin_document_bytes(&bytes)
}

fn verify_pin_document_bytes(bytes: &[u8]) -> Result<()> {
    let doc: PinDocument = serde_json::from_slice(bytes)
        .map_err(|e| VerificationError::new(ErrorCode::Json, format!("pin.json: {e}")))?;
    let expected = PinDocument::expected();
    if doc.npm_specifier != expected.npm_specifier
        || doc.npm_integrity != expected.npm_integrity
        || doc.compiler_repository != expected.compiler_repository
        || doc.compiler_commit != expected.compiler_commit
        || doc.compiler_tag != expected.compiler_tag
        || doc.suite_repository != expected.suite_repository
        || doc.suite_commit != expected.suite_commit
    {
        return Err(VerificationError::new(
            ErrorCode::ProvenanceMismatch,
            "oracle/pin.json does not match pinned TypeScript 7.0.2 identity",
        ));
    }
    // Touch constants so pin drift against module constants is obvious to readers.
    let _ = (
        NPM_SPECIFIER,
        NPM_INTEGRITY,
        COMPILER_REPOSITORY,
        COMPILER_COMMIT,
        COMPILER_TAG,
        SUITE_REPOSITORY,
        SUITE_COMMIT,
    );
    Ok(())
}

fn case_inputs_from_index(index: &SuiteIndex) -> BTreeSet<String> {
    index
        .entries
        .values()
        .filter(|entry| matches!(entry.asset_kind, AssetKind::CaseInput))
        .map(|entry| entry.logical_path.clone())
        .collect()
}

fn store_blob(dir: &Path, digest_hex: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(digest_hex);
    if path.exists() {
        let existing = fs::read(&path).map_err(|e| io_err(&path, &e))?;
        if existing != bytes {
            return Err(VerificationError::new(
                ErrorCode::Digest,
                format!("content mismatch for blob `{digest_hex}`"),
            ));
        }
        return Ok(());
    }
    write_then_verify(&path, bytes)
}

fn write_then_verify(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
    }
    fs::write(path, bytes).map_err(|e| io_err(path, &e))?;
    let read_back = fs::read(path).map_err(|e| io_err(path, &e))?;
    if read_back != bytes {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!("write-then-verify failed for `{}`", path.display()),
        ));
    }
    Ok(())
}

pub(crate) fn fetch_archive(url: &str, expected_digest: &str, cache_dir: &Path) -> Result<PathBuf> {
    let cache_path = cache_dir.join(format!("{expected_digest}.tar.gz"));
    if cache_path.exists() {
        let bytes = fs::read(&cache_path).map_err(|e| io_err(&cache_path, &e))?;
        let actual = sha256_hex(&bytes);
        if actual == expected_digest {
            return Ok(cache_path);
        }
        fs::remove_file(&cache_path).map_err(|e| io_err(&cache_path, &e))?;
    }

    let status = Command::new("curl")
        .args(["-fsSL", "--output"])
        .arg(&cache_path)
        .arg(url)
        .status()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                VerificationError::new(ErrorCode::ToolMissing, "curl is not available on PATH")
            } else {
                VerificationError::new(
                    ErrorCode::ToolFailed,
                    format!("failed to spawn curl: {error}"),
                )
            }
        })?;
    if !status.success() {
        let _ = fs::remove_file(&cache_path);
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("curl failed fetching `{url}` with {status}"),
        ));
    }
    let bytes = fs::read(&cache_path).map_err(|e| io_err(&cache_path, &e))?;
    let actual = sha256_hex(&bytes);
    if actual != expected_digest {
        let _ = fs::remove_file(&cache_path);
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "archive digest mismatch for `{url}`: expected `{expected_digest}`, got `{actual}`"
            ),
        ));
    }
    Ok(cache_path)
}

pub(crate) fn extract_archive(archive: &Path) -> Result<TempDir> {
    // Each archive extracts into its own temp dir. Without a per-archive
    // discriminator the suite and compiler extractions collide on the same
    // path (same prefix + pid), and the second extraction wipes the first.
    let discriminator = &sha256_hex(archive.to_string_lossy().as_bytes())[..8];
    let temp = TempDir::new(&format!("bamts-suite-extract-{discriminator}"))?;
    let listed = Command::new("tar")
        .args(["-tzf"])
        .arg(archive)
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                VerificationError::new(ErrorCode::ToolMissing, "tar is not available on PATH")
            } else {
                VerificationError::new(
                    ErrorCode::ToolFailed,
                    format!("failed to spawn tar: {error}"),
                )
            }
        })?;
    if !listed.status.success() {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "tar -tzf failed: {}",
                String::from_utf8_lossy(&listed.stderr)
            ),
        ));
    }
    let listing = String::from_utf8_lossy(&listed.stdout);
    for member in listing.lines() {
        reject_tar_member(member)?;
    }

    let verbose = Command::new("tar")
        .args(["-tvzf"])
        .arg(archive)
        .output()
        .map_err(|error| {
            VerificationError::new(
                ErrorCode::ToolFailed,
                format!("failed to spawn tar verbose list: {error}"),
            )
        })?;
    if !verbose.status.success() {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "tar -tvzf failed: {}",
                String::from_utf8_lossy(&verbose.stderr)
            ),
        ));
    }
    let verbose_listing = String::from_utf8_lossy(&verbose.stdout);
    for line in verbose_listing.lines() {
        reject_tar_verbose_line(line)?;
    }

    let status = Command::new("tar")
        .args([
            "-xzf",
            archive.to_str().ok_or_else(|| {
                VerificationError::new(ErrorCode::Io, "archive path is not UTF-8")
            })?,
            "--no-same-owner",
            "--no-same-permissions",
            "-C",
        ])
        .arg(temp.path())
        .status()
        .map_err(|error| {
            VerificationError::new(
                ErrorCode::ToolFailed,
                format!("failed to spawn tar extract: {error}"),
            )
        })?;
    if !status.success() {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("tar extract failed with {status}"),
        ));
    }
    reject_extracted_symlinks(temp.path())?;
    Ok(temp)
}

fn reject_tar_member(member: &str) -> Result<()> {
    if member.is_empty() {
        return Ok(());
    }
    if member.starts_with('/') || member.contains("://") {
        return Err(VerificationError::new(
            ErrorCode::ProvenanceMismatch,
            format!("archive member absolute path rejected: `{member}`"),
        ));
    }
    for part in member.split('/') {
        if part == ".." {
            return Err(VerificationError::new(
                ErrorCode::ProvenanceMismatch,
                format!("archive member traversal rejected: `{member}`"),
            ));
        }
    }
    Ok(())
}

fn reject_tar_verbose_line(line: &str) -> Result<()> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return Ok(());
    }
    if matches!(trimmed.chars().next(), Some('-') | Some('d')) {
        return Ok(());
    }
    Err(VerificationError::new(
        ErrorCode::ProvenanceMismatch,
        format!("archive member link/special type rejected: `{line}`"),
    ))
}

fn member_looks_allowlisted(member: &str) -> bool {
    let lower = member.to_ascii_lowercase().replace('\\', "/");
    if lower.contains("tests/cases/")
        || lower.contains("tests/baselines/")
        || lower.contains("testdata/")
    {
        return true;
    }
    let base = Path::new(&lower)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    base.starts_with("license") || base.starts_with("notice")
}

fn reject_extracted_symlinks(root: &Path) -> Result<()> {
    // Do not fail the whole archive on incidental symlinks outside imported
    // paths; walk/import refuse to follow or read them.
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) => return Err(io_err(&current, &error)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&current, &e))?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| io_err(&path, &e))?;
            if meta.file_type().is_symlink() {
                return Err(VerificationError::new(
                    ErrorCode::ProvenanceMismatch,
                    format!("extracted symlink rejected: `{}`", path.display()),
                ));
            }
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

pub(crate) fn single_archive_root(extract_root: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(extract_root).map_err(|e| io_err(extract_root, &e))? {
        let entry = entry.map_err(|e| io_err(extract_root, &e))?;
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| io_err(&entry.path(), &e))?;
        if meta.is_dir() {
            dirs.push(entry.path());
        }
    }
    if dirs.len() != 1 {
        return Err(VerificationError::new(
            ErrorCode::ProvenanceMismatch,
            format!(
                "expected exactly one archive root under `{}`, found {}",
                extract_root.display(),
                dirs.len()
            ),
        ));
    }
    Ok(dirs.remove(0))
}

pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|e| io_err(&path, &e))?;
        }
        fs::create_dir_all(&path).map_err(|e| io_err(&path, &e))?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn canonicalize_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut json = serde_json::to_value(value)
        .map_err(|e| VerificationError::new(ErrorCode::Json, format!("{e}")))?;
    sort_json_keys(&mut json);
    let text = serde_json::to_string_pretty(&json)
        .map_err(|e| VerificationError::new(ErrorCode::Json, format!("{e}")))?;
    let canonical = text
        .lines()
        .map(|line| line.trim_end().replace('\r', ""))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(canonical.into_bytes())
}

fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.sort_keys();
            for member in map.values_mut() {
                sort_json_keys(member);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(Sha256::digest(bytes))
}

fn sha256_hex_concat(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hex_encode(hasher.finalize())
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn io_err(path: &Path, error: &io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

/// Materialize from an extracted tree without network (test seam).
pub fn materialize_from_extracted(
    snapshot_root: &Path,
    extracted_suite_root: &Path,
) -> Result<SuiteSnapshot> {
    sync_suite(&SyncOptions {
        verify_pin: true,
        write_snapshot: true,
        workspace_root: PathBuf::from("."),
        snapshot_root: snapshot_root.to_path_buf(),
        extracted_suite_root: Some(extracted_suite_root.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bamts-u04-{label}-{}-{}",
                std::process::id(),
                sha256_hex(label.as_bytes())[..8].to_owned()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_fixture_tree(root: &Path) {
        let case = root.join("tests/cases/compiler/a.ts");
        fs::create_dir_all(case.parent().unwrap()).unwrap();
        fs::write(&case, b"export const answer = 42;\n").unwrap();
        let baseline = root.join("tests/baselines/reference/a.errors.txt");
        fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        fs::write(&baseline, b"error TS0000: sample\n").unwrap();
        fs::write(root.join("LICENSE"), b"Apache-2.0\n").unwrap();
        fs::write(root.join("NOTICE"), b"notice\n").unwrap();
        // Noise that must be ignored / rejected by allowlist.
        fs::create_dir_all(root.join("hereby")).unwrap();
        fs::write(root.join("hereby/task.txt"), b"nope\n").unwrap();
        fs::create_dir_all(root.join("internal/testutil")).unwrap();
        fs::write(root.join("internal/testutil/x.ts"), b"nope\n").unwrap();
    }

    /// A minimal index exercising the U2.8 Phase B `types` ownership rule:
    /// `owned` (sole stem + `.types`), `nobaseline` (no `.types`), and `dup`
    /// (a stem shared by two case inputs, so unownable from the index alone).
    fn s2_test_index() -> SuiteIndex {
        fn entry(path: &str, kind: AssetKind) -> IndexEntry {
            IndexEntry {
                logical_path: path.to_owned(),
                sha256: "0".repeat(64),
                asset_kind: kind,
                facet: None,
                partition: None,
            }
        }
        let mut entries = BTreeMap::new();
        for (path, kind) in [
            ("tests/cases/compiler/owned.ts", AssetKind::CaseInput),
            (
                "tests/baselines/reference/owned.types",
                AssetKind::BaselineFacet,
            ),
            ("tests/cases/compiler/nobaseline.ts", AssetKind::CaseInput),
            ("tests/cases/compiler/dup.ts", AssetKind::CaseInput),
            ("tests/cases/conformance/dup.ts", AssetKind::CaseInput),
            (
                "tests/baselines/reference/dup.types",
                AssetKind::BaselineFacet,
            ),
            (
                "tests/baselines/reference/owned.symbols",
                AssetKind::BaselineFacet,
            ),
            (
                "tests/baselines/reference/dup.symbols",
                AssetKind::BaselineFacet,
            ),
        ] {
            entries.insert(path.to_owned(), entry(path, kind));
        }
        SuiteIndex { entries }
    }

    #[test]
    fn snapshot_determinism_across_discovery_order() {
        let a = TestDir::new("det-a");
        let b = TestDir::new("det-b");
        write_fixture_tree(a.path());
        write_fixture_tree(b.path());
        // Add an extra file in different relative creation order on b.
        fs::write(b.path().join("tests/cases/compiler/z.ts"), b"export {};\n").unwrap();
        fs::write(a.path().join("tests/cases/compiler/z.ts"), b"export {};\n").unwrap();

        let snap_a = TestDir::new("snap-a");
        let snap_b = TestDir::new("snap-b");
        let left = materialize_from_extracted(snap_a.path(), a.path()).unwrap();
        let right = materialize_from_extracted(snap_b.path(), b.path()).unwrap();
        assert_eq!(left.digest, right.digest);
        assert_eq!(
            fs::read(snap_a.path().join("index.json")).unwrap(),
            fs::read(snap_b.path().join("index.json")).unwrap()
        );
        assert_eq!(
            fs::read(snap_a.path().join("ledger.json")).unwrap(),
            fs::read(snap_b.path().join("ledger.json")).unwrap()
        );
    }

    #[test]
    fn ledger_completeness_reports_ledger_incomplete() {
        let suite = TestDir::new("complete-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("complete-snap");
        let snapshot = materialize_from_extracted(snap.path(), suite.path()).unwrap();
        let mut discovered = case_inputs_from_index(&snapshot.index);
        discovered.insert("tests/cases/compiler/missing.ts".to_owned());
        let err = audit_ledger(&snap.path().join("ledger.json"), true, &discovered)
            .expect_err("missing input must fail");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("LEDGER_INCOMPLETE:"));
    }

    #[test]
    fn shard_key_stability_golden() {
        let key = compute_shard_key(
            "tests/cases/compiler/a.ts#parse",
            Facet::Parse,
            &[Backend::Check],
        );
        assert_eq!(key.len(), 16);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let again = compute_shard_key(
            "tests/cases/compiler/a.ts#parse",
            Facet::Parse,
            &[Backend::Check],
        );
        assert_eq!(key, again);
        // Order independence of backends list.
        let key_sorted = compute_shard_key(
            "id#jsEmit",
            Facet::JsEmit,
            &[Backend::Aot, Backend::Interpreter, Backend::Jit],
        );
        let key_unsorted = compute_shard_key(
            "id#jsEmit",
            Facet::JsEmit,
            &[Backend::Jit, Backend::Aot, Backend::Interpreter],
        );
        assert_eq!(key_sorted, key_unsorted);
        let n = 4;
        let index = shard_index(&key, n).unwrap();
        assert!(index < n);
    }

    #[test]
    fn invalid_shard_flag_rejected() {
        assert!(parse_shards("0/4").is_err());
        assert!(parse_shards("5/4").is_err());
        assert!(parse_shards("1/0").is_err());
        assert!(parse_shards("abc").is_err());
        assert_eq!(parse_shards("1/1").unwrap(), (1, 1));
        assert_eq!(parse_shards("2/3").unwrap(), (2, 3));
    }

    #[test]
    fn digest_mismatch_rejected_before_execution() {
        let suite = TestDir::new("digest-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("digest-snap");
        materialize_from_extracted(snap.path(), suite.path()).unwrap();
        fs::write(snap.path().join("snapshot.sha256"), b"00\n").unwrap();
        let err = run_suite(
            Path::new("."),
            snap.path(),
            &RunFilterOptions {
                status: StatusFilter::All,
                slice: None,
                backends: BackendFilter::All,
                shards: None,
            },
        )
        .expect_err("tampered digest must fail before execute");
        assert_eq!(err.code(), ErrorCode::ProvenanceMismatch);
    }

    #[test]
    fn timeout_class_default_and_cap() {
        assert_eq!(budget_for(TimeoutClass::Frontend), (5_000, 30_000));
        assert_eq!(budget_for(TimeoutClass::Execute), (10_000, 60_000));
        assert_eq!(budget_for(TimeoutClass::Project), (30_000, 60_000));
        assert_eq!(budget_for(TimeoutClass::Watch), (30_000, 60_000));
    }

    #[test]
    fn output_truncation_over_1mib() {
        let oversized = vec![b'x'; OUTPUT_CAP_BYTES + 8];
        let (bounded, truncated) = bounded_output(oversized, OUTPUT_CAP_BYTES);
        assert!(truncated);
        assert_eq!(bounded.len(), OUTPUT_CAP_BYTES);
        assert_eq!(
            classify_truncation(true, false),
            Some(FailureClass::OutputTruncated)
        );
        assert_eq!(classify_truncation(false, false), None);
    }

    #[test]
    fn aot_scratch_over_1gib_fails_and_cleans_up() {
        let root = TestDir::new("aot-root");
        let spec = scratch_case_spec("scratch_case");
        let artifacts = ArtifactDirectory::create(root.path(), &spec, ExecutionMode::Aot).unwrap();
        let path = artifacts.0.clone();
        fs::write(path.join("ok.bin"), b"tiny").unwrap();
        enforce_aot_scratch_cap(&path).unwrap();
        drop(artifacts);
        assert!(!path.exists(), "ArtifactDirectory Drop must clean scratch");

        let manual = root.path().join("manual-scratch");
        fs::create_dir_all(&manual).unwrap();
        fs::write(manual.join("marker"), b"x").unwrap();
        let err = enforce_aot_scratch_size(&manual, AOT_SCRATCH_CAP_BYTES + 1)
            .expect_err("over-cap scratch must fail");
        assert_eq!(err.code(), ErrorCode::ToolFailed);
        assert!(!manual.exists(), "failed over-cap path must be cleaned");
    }

    #[test]
    fn allowlist_rejects_hereby_traversal_testutil() {
        assert!(classify_logical_path("hereby/task.txt").unwrap().is_none());
        assert!(
            classify_logical_path("internal/testutil/x.ts")
                .unwrap()
                .is_none()
        );
        assert!(classify_logical_path("tests/cases/compiler/../secret.ts").is_err());
        assert!(classify_logical_path("/abs/tests/cases/compiler/a.ts").is_err());
        let ok = classify_logical_path("tests/cases/compiler/a.ts")
            .unwrap()
            .expect("case input");
        assert!(matches!(ok.0, AssetKind::CaseInput));
        assert!(matches!(ok.1, Some(Partition::Compiler)));
    }

    #[test]
    fn provisional_ledger_maps_all_15_facets() {
        let suite = TestDir::new("facets-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("facets-snap");
        let snapshot = materialize_from_extracted(snap.path(), suite.path()).unwrap();
        let input = "tests/cases/compiler/a.ts";
        let mut facets = BTreeSet::new();
        for entry in &snapshot.ledger.entries {
            if entry.input == input {
                facets.insert(entry.facet);
                match entry.facet {
                    Facet::Harness => {
                        assert!(matches!(entry.status, Status::Excluded));
                        assert!(matches!(entry.reason_code, ReasonCode::UpstreamHarnessOnly));
                        assert!(entry.blocked_by.is_empty());
                    }
                    Facet::Implementation => {
                        assert!(matches!(entry.status, Status::Excluded));
                        assert!(matches!(
                            entry.reason_code,
                            ReasonCode::UpstreamImplementationOnly
                        ));
                    }
                    Facet::Parse
                    | Facet::Diagnostics
                    | Facet::Types
                    | Facet::Symbols
                    | Facet::JsEmit
                    | Facet::DtsEmit
                    | Facet::ModuleResolution
                    | Facet::Config
                    | Facet::Cli
                    | Facet::Build
                    | Facet::Watch
                    | Facet::NodeApi
                    | Facet::LanguageService => {
                        assert!(matches!(entry.status, Status::Deferred));
                        assert_eq!(entry.blocked_by, vec!["S0".to_owned()]);
                        assert!(matches!(
                            entry.reason_code,
                            ReasonCode::PromisedNotImplemented
                        ));
                    }
                }
                assert_eq!(entry.evidence[0].tier, 1);
            }
        }
        assert_eq!(facets.len(), 15);
    }

    #[test]
    fn deferred_excluded_rollup_skip_classes() {
        let suite = TestDir::new("rollup-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("rollup-snap");
        materialize_from_extracted(snap.path(), suite.path()).unwrap();
        let report = run_suite(
            Path::new("."),
            snap.path(),
            &RunFilterOptions {
                status: StatusFilter::All,
                slice: None,
                backends: BackendFilter::All,
                shards: Some((1, 1)),
            },
        )
        .unwrap();
        assert!(
            report
                .rollups
                .get(&FailureClass::SkipDeferred)
                .copied()
                .unwrap_or(0)
                > 0
        );
        assert!(
            report
                .rollups
                .get(&FailureClass::SkipExcluded)
                .copied()
                .unwrap_or(0)
                > 0
        );
        assert_eq!(report.state_reached, RunState::Publish);
    }

    #[test]
    fn fresh_sync_keeps_the_committed_classified_ledger_authoritative() {
        let suite = TestDir::new("classified-sync-suite");
        write_fixture_tree(suite.path());
        let snapshot = TestDir::new("classified-sync-snapshot");
        materialize_from_extracted(snapshot.path(), suite.path()).unwrap();
        let workspace = TestDir::new("classified-sync-workspace");
        let ledger_path = workspace.path().join("verification/ts-suite-ledger.json");
        write_suite_ledger(snapshot.path(), &ledger_path).unwrap();

        // A later pin/snapshot refresh recreates the provisional embedded ledger.
        materialize_from_extracted(snapshot.path(), suite.path()).unwrap();

        let report = run_suite(
            workspace.path(),
            snapshot.path(),
            &RunFilterOptions {
                status: StatusFilter::Included,
                slice: Some("s1".to_owned()),
                backends: BackendFilter::One(Backend::Check),
                shards: Some((1, 1)),
            },
        )
        .unwrap();

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].facet, Facet::Parse);
        assert!(matches!(report.results[0].class, FailureClass::Pass));
    }
    #[test]
    fn committed_ledger_must_match_the_materialized_index() {
        let suite = TestDir::new("classified-digest-suite");
        write_fixture_tree(suite.path());
        let snapshot = TestDir::new("classified-digest-snapshot");
        materialize_from_extracted(snapshot.path(), suite.path()).unwrap();
        let workspace = TestDir::new("classified-digest-workspace");
        let ledger_path = workspace.path().join("verification/ts-suite-ledger.json");
        let mut ledger = write_suite_ledger(snapshot.path(), &ledger_path).unwrap();
        ledger.snapshot.digest = "0".repeat(64);
        fs::write(&ledger_path, TsLedgerWriter::to_vec(&ledger).unwrap()).unwrap();
        materialize_from_extracted(snapshot.path(), suite.path()).unwrap();

        let error = run_suite(
            workspace.path(),
            snapshot.path(),
            &RunFilterOptions {
                status: StatusFilter::Included,
                slice: Some("s1".to_owned()),
                backends: BackendFilter::One(Backend::Check),
                shards: Some((1, 1)),
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::Digest);
    }

    #[test]
    fn included_s1_parse_rows_are_selected_by_slice_and_pass_check() {
        let suite = TestDir::new("s1parse-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("s1parse-snap");
        materialize_from_extracted(snap.path(), suite.path()).unwrap();
        // Reclassify so the snapshot's own ledger carries the U1.3 included
        // `parse` rows the runner reads.
        let ledger_out = snap.path().join("ts-suite-ledger.json");
        write_suite_ledger(snap.path(), &ledger_out).unwrap();

        let report = run_suite(
            Path::new("."),
            snap.path(),
            &RunFilterOptions {
                status: StatusFilter::Included,
                slice: Some("s1".to_owned()),
                backends: BackendFilter::One(Backend::Check),
                shards: Some((1, 1)),
            },
        )
        .unwrap();

        // The single fixture case yields exactly one included parse cell, and it
        // parses/checks cleanly, so the only rollup class is Pass.
        assert!(!report.results.is_empty(), "slice s1 selected no cells");
        assert!(
            report.results.iter().all(|r| r.facet == Facet::Parse),
            "slice s1 must select only parse-facet cells"
        );
        assert!(
            report
                .results
                .iter()
                .all(|r| matches!(r.class, FailureClass::Pass)),
            "every included S1 parse cell must pass: {:?}",
            report.results
        );
        assert_eq!(report.state_reached, RunState::Publish);
    }

    #[test]
    fn no_silent_skip_when_included_result_missing() {
        let entry = provisional_entry(
            "tests/cases/compiler/a.ts",
            Partition::Compiler,
            Facet::Parse,
        );
        let mut included = entry;
        included.status = Status::Included;
        included.reason_code = ReasonCode::PromisedCompilerContract;
        included.blocked_by.clear();
        let planned = vec![PlannedCell {
            entry: included,
            backend: Backend::Check,
        }];
        let err = enforce_no_silent_skip(&planned, &[]).expect_err("missing included result");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("LEDGER_INCOMPLETE:"));
        assert!(err.to_string().contains("NO_SILENT_SKIP"));
        let _ = status_name(Status::Included);
    }

    #[test]
    fn classify_cell_follows_section_17_2() {
        // Facet-level exclusions win over any partition, including special ones.
        for partition in [
            Partition::Compiler,
            Partition::Transpile,
            Partition::UnitTests,
        ] {
            let (status, reason, blocked) = classify_cell(partition, Facet::Harness);
            assert_eq!(status, Status::Excluded);
            assert_eq!(reason, ReasonCode::UpstreamHarnessOnly);
            assert!(blocked.is_empty());
            let (status, reason, blocked) = classify_cell(partition, Facet::Implementation);
            assert_eq!(status, Status::Excluded);
            assert_eq!(reason, ReasonCode::UpstreamImplementationOnly);
            assert!(blocked.is_empty());
        }

        // Compiler/conformance facet-partitioned owning slices. `parse` (U1.3)
        // and `diagnostics` (U2.8) are included compiler contracts (asserted
        // separately below); every other facet remains deferred to its later
        // owning slice.
        let expected: &[(Facet, &str)] = &[
            (Facet::Types, "S2"),
            (Facet::Symbols, "S2"),
            (Facet::JsEmit, "S4"),
            (Facet::DtsEmit, "S4"),
            (Facet::ModuleResolution, "S5"),
            (Facet::Config, "S7"),
            (Facet::Cli, "S7"),
            (Facet::Build, "S8"),
            (Facet::Watch, "S9"),
            (Facet::NodeApi, "S10"),
            (Facet::LanguageService, "S11"),
        ];
        for (facet, slice) in expected {
            let (status, reason, blocked) = classify_cell(Partition::Compiler, *facet);
            assert_eq!(status, Status::Deferred, "facet {}", facet.as_str());
            assert_eq!(reason, ReasonCode::PromisedNotImplemented);
            assert_eq!(
                blocked,
                vec![(*slice).to_owned()],
                "facet {}",
                facet.as_str()
            );
        }

        // U1.3: `parse` is an included compiler contract across every general
        // partition, with no blocking slice.
        for partition in [
            Partition::Compiler,
            Partition::Conformance,
            Partition::Project,
            Partition::Projects,
            Partition::Other,
        ] {
            assert_eq!(
                classify_cell(partition, Facet::Parse),
                (
                    Status::Included,
                    ReasonCode::PromisedCompilerContract,
                    vec![]
                ),
                "parse facet in general partition"
            );
        }

        // U2.8: `diagnostics` is an included compiler contract across every
        // general partition; `classify_s2_entry` re-defers project rows to S5
        // and excludes projects rows (per-input, asserted below).
        for partition in [
            Partition::Compiler,
            Partition::Conformance,
            Partition::Project,
            Partition::Projects,
            Partition::Other,
        ] {
            assert_eq!(
                classify_cell(partition, Facet::Diagnostics),
                (
                    Status::Included,
                    ReasonCode::PromisedCompilerContract,
                    vec![]
                ),
                "diagnostics facet in general partition"
            );
        }
        let s2 = crate::check_cells::S2Classification::from_index(&s2_test_index());
        assert_eq!(
            classify_s2_entry(
                "tests/cases/project/foo.json",
                Partition::Project,
                Facet::Diagnostics,
                &s2,
            ),
            Some((
                Status::Deferred,
                ReasonCode::PromisedNotImplemented,
                vec!["S5".to_owned()]
            )),
            "project diagnostics re-defer to S5 (module-resolution baselines)"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/projects/foo.ts",
                Partition::Projects,
                Facet::Diagnostics,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "projects rows carry no upstream baseline artifacts"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/foo.ts",
                Partition::Compiler,
                Facet::Diagnostics,
                &s2,
            ),
            None,
            "compiler diagnostics keep the generic included rule"
        );
        // Phase B `types` per-input ownership.
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/owned.ts",
                Partition::Compiler,
                Facet::Types,
                &s2,
            ),
            Some((
                Status::Included,
                ReasonCode::PromisedCompilerContract,
                vec![]
            )),
            "a sole-stem compiler input owning a `.types` baseline is included"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/nobaseline.ts",
                Partition::Compiler,
                Facet::Types,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "a compiler input with no `.types` baseline is excluded"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/dup.ts",
                Partition::Compiler,
                Facet::Types,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "a duplicate-stem input cannot be proven owner from the index alone"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/project/foo.json",
                Partition::Project,
                Facet::Types,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "project types rows have no upstream `.types` baselines"
        );

        // Phase C `symbols` per-input ownership (identical rule to `types`).
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/owned.ts",
                Partition::Compiler,
                Facet::Symbols,
                &s2,
            ),
            Some((
                Status::Included,
                ReasonCode::PromisedCompilerContract,
                vec![]
            )),
            "a sole-stem compiler input owning a `.symbols` baseline is included"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/nobaseline.ts",
                Partition::Compiler,
                Facet::Symbols,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "a compiler input with no `.symbols` baseline is excluded"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/compiler/dup.ts",
                Partition::Compiler,
                Facet::Symbols,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "a duplicate-stem input cannot be proven owner from the index alone"
        );
        assert_eq!(
            classify_s2_entry(
                "tests/cases/projects/foo.ts",
                Partition::Projects,
                Facet::Symbols,
                &s2,
            ),
            Some((Status::Excluded, ReasonCode::UpstreamHarnessOnly, vec![])),
            "projects symbols rows have no upstream `.symbols` baselines"
        );

        // Whole-partition dispositions (over the per-facet fallback).
        assert_eq!(
            classify_cell(Partition::Transpile, Facet::Parse),
            (
                Status::Deferred,
                ReasonCode::PromisedNotImplemented,
                vec!["S10".to_owned()]
            )
        );
        assert_eq!(
            classify_cell(Partition::Fourslash, Facet::Types),
            (
                Status::Deferred,
                ReasonCode::PromisedNotImplemented,
                vec!["S11".to_owned()]
            )
        );
        assert_eq!(
            classify_cell(Partition::TscWatch, Facet::Diagnostics),
            (
                Status::Deferred,
                ReasonCode::PromisedNotImplemented,
                vec!["S9".to_owned()]
            )
        );
        // Upstream unit tests: excluded with an implementation reason code.
        let (status, reason, blocked) = classify_cell(Partition::UnitTests, Facet::Parse);
        assert_eq!(status, Status::Excluded);
        assert_eq!(reason, ReasonCode::UpstreamImplementationOnly);
        assert!(blocked.is_empty());
    }

    #[test]
    fn classified_ledger_v1_is_valid_and_digest_bound() {
        let suite = TestDir::new("v1-suite");
        write_fixture_tree(suite.path());
        let snap = TestDir::new("v1-snap");
        materialize_from_extracted(snap.path(), suite.path()).unwrap();

        let ledger_out = snap.path().join("ts-suite-ledger.json");
        let ledger = write_suite_ledger(snap.path(), &ledger_out).unwrap();

        // Re-reading through the schema-bound reader must succeed.
        let reparsed = TsLedgerReader::from_file(&ledger_out).unwrap();
        assert_eq!(reparsed, ledger);
        assert_eq!(ledger.schema_version, 1);

        // snapshot.digest is the index content digest, which is independent
        // of the top-level snapshot.sha256 manifest digest.
        let verified = verify_snapshot(snap.path()).unwrap();
        assert_eq!(
            ledger.snapshot.digest,
            index_content_digest(&verified.snapshot.index)
        );
        // One fixture case × 15 facets: harness+implementation excluded, plus
        // `types` and `symbols` excluded (no `.types`/`.symbols` baseline for the
        // fixture stem); the `parse` (U1.3) and `diagnostics` (U2.8) facets are
        // included compiler contracts; the rest remain deferred to their later
        // owning slices.
        assert_eq!(ledger.totals.entries, 15);
        assert_eq!(ledger.totals.included, 2);
        assert_eq!(ledger.totals.excluded, 4);
        assert_eq!(ledger.totals.deferred, 9);
        assert_eq!(ledger.totals.discovered_inputs, 1);
        assert_eq!(
            ledger.totals.included + ledger.totals.deferred + ledger.totals.excluded,
            ledger.totals.entries
        );

        // Every row carries a reason code and at least one evidence record.
        for entry in &ledger.entries {
            assert!(!entry.evidence.is_empty());
            match entry.facet {
                Facet::Harness => {
                    assert_eq!(entry.status, Status::Excluded);
                    assert_eq!(entry.reason_code, ReasonCode::UpstreamHarnessOnly);
                }
                Facet::Implementation => {
                    assert_eq!(entry.status, Status::Excluded);
                    assert_eq!(entry.reason_code, ReasonCode::UpstreamImplementationOnly);
                }
                Facet::Parse | Facet::Diagnostics => {
                    assert_eq!(entry.status, Status::Included);
                    assert_eq!(entry.reason_code, ReasonCode::PromisedCompilerContract);
                    assert!(entry.blocked_by.is_empty());
                }
                Facet::Types | Facet::Symbols => {
                    assert_eq!(entry.status, Status::Excluded);
                    assert_eq!(entry.reason_code, ReasonCode::UpstreamHarnessOnly);
                }
                _ => {
                    assert_eq!(entry.status, Status::Deferred);
                    assert!(!entry.blocked_by.is_empty());
                }
            }
        }
    }

    #[test]
    fn tar_preflight_rejects_non_allowlisted_symlink_and_hard_link() {
        let symlink = "lrwxrwxrwx owner/group 0 2026-08-20 00:00 package/docs/link -> target";
        let hard_link =
            "hrw-r--r-- owner/group 0 2026-08-20 00:00 package/docs/alias link to target";
        assert_eq!(
            reject_tar_verbose_line(symlink).unwrap_err().code,
            ErrorCode::ProvenanceMismatch
        );
        assert_eq!(
            reject_tar_verbose_line(hard_link).unwrap_err().code,
            ErrorCode::ProvenanceMismatch
        );
    }

    #[test]
    fn tar_preflight_allows_only_regular_files_and_directories() {
        reject_tar_verbose_line("-rw-r--r-- owner/group 1 2026-08-20 00:00 package/docs/file")
            .unwrap();
        reject_tar_verbose_line("drwxr-xr-x owner/group 0 2026-08-20 00:00 package/docs/").unwrap();
        for special in ['c', 'b', 'p', 's'] {
            let line = format!("{special}rw-r--r-- owner/group 0 date package/docs/special");
            assert!(reject_tar_verbose_line(&line).is_err());
        }
    }
}
