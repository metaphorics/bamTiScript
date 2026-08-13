//! Strict parsing and validation for the vendored corpus manifest and per-case
//! specs, plus bounded raw-byte Node and BamTS differential runners.
//!
//! This module owns the corpus verification contract:
//!
//! * `corpus/manifest.toml` and each `corpus/specs/<id>.toml` are parsed with
//!   `deny_unknown_fields`, then validated for exact schema, pins, and clean
//!   relative paths.  The manifest's declared `environment` and `compare` sets
//!   must match the canonical normalization exactly.
//! * The oracle spawns the pinned Node interpreter (exactly `24.18.0`) directly
//!   on a case entrypoint.  It normalizes the environment to exactly
//!   `TZ=UTC`, `LANG=C`, `LC_ALL=C`, `NO_COLOR=1`, captures raw stdout bytes and
//!   the exact exit code as the parity key, and treats stderr as evidence only.
//!   Output is bounded and execution is killed after a per-case timeout.
//!
//! The oracle never invokes package managers, project scripts, or transpilers:
//! it runs `node <entrypoint>` and relies on the interpreter's own execution.
//! The BamTS runner executes the same validated entrypoint through the public
//! CLI driver in JIT mode, compiles, links, and spawns it in AOT mode, or — in
//! Interpreter mode, without any shell-out — compiles it through the public
//! `bamts` facade and executes it in-process via `bamts_runtime::run` against
//! a Node host.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::AtomicUsize,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use bamts_cli::{
    args::{CliArgs, ExecutionTarget, Mode, parse_args},
    driver,
};
use bamts_runtime::Limits;

use crate::{ErrorCode, Result, VerificationError};

/// Canonical corpus schema version accepted by this validator.
pub const SCHEMA_VERSION: u32 = 1;
/// Exact Node version the oracle is pinned to.
pub const NODE_VERSION: &str = "24.18.0";
/// The exact stdout `node --version` must report.
pub const NODE_VERSION_OUTPUT: &str = "v24.18.0";
/// Repository-relative path to the corpus manifest.
pub const MANIFEST_PATH: &str = "corpus/manifest.toml";

/// The complete pinned corpus, in manifest order.
pub const PINNED_CASE_IDS: [&str; 20] = [
    "citty",
    "defu",
    "destr",
    "dot-prop",
    "escape-string-regexp",
    "hookable",
    "is-plain-obj",
    "mitt",
    "ohash",
    "p-defer",
    "p-map",
    "p-queue",
    "pathe",
    "perfect-debounce",
    "rou3",
    "tiny-invariant",
    "tslib",
    "ufo",
    "valita",
    "yocto-queue",
];

/// Synchronous corpus cases unblocked by Task 106 alone, in manifest order.
pub const TASK_106_SYNC_CASE_IDS: [&str; 11] = [
    "defu",
    "destr",
    "dot-prop",
    "escape-string-regexp",
    "mitt",
    "p-queue",
    "pathe",
    "rou3",
    "tslib",
    "ufo",
    "valita",
];

/// Pinned Node builtin cases completed by Task 107, in manifest order.
pub const TASK_107_NODE_CASE_IDS: [&str; 3] = ["citty", "is-plain-obj", "ohash"];

/// The only environment variables the oracle exposes to a case.  Everything
/// inherited from the parent process is cleared before these are set.
pub const NORMALIZED_ENV: [&str; 4] = ["TZ=UTC", "LANG=C", "LC_ALL=C", "NO_COLOR=1"];
/// Exact comparison keys the manifest must declare.
pub const COMPARE_KEYS: [&str; 2] = ["stdout", "exit_code"];

const COMMIT_LEN: usize = 40;
const MIN_TIMEOUT_MS: u64 = 1;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1 << 20;
const READ_CHUNK: usize = 8192;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const NODE_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const NODE_VERSION_OUTPUT_CAP: usize = 128;
const INTERPRETER_FUEL_PER_MILLISECOND: u64 = 10_000;
const CORPUS_WORKER_REQUEST: &str = "BAMTS_CORPUS_WORKER_REQUEST";
const CORPUS_WORKER_TEST: &str = "corpus_differential_worker";
const HARNESS_OWNED_ARGS: &[&str] = &[
    "check",
    "compile",
    "run",
    "explain",
    "-c",
    "--compile",
    "-r",
    "--run",
    "--check",
    "aot",
    "jit",
    "--aot",
    "--jit",
    "-t",
    "--target",
    "-o",
    "--output",
    "--out-dir",
    "--output-dir",
    "--js-compat",
];

// ---------------------------------------------------------------------------
// Validated records
// ---------------------------------------------------------------------------

/// A validated view of `corpus/manifest.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusManifest {
    pub node_version: String,
    pub environment: Vec<String>,
    pub compare: Vec<String>,
    pub projects: Vec<ManifestProject>,
}

/// A single manifest project entry.  Paths are guaranteed clean and relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestProject {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub spec: String,
    pub entrypoint: String,
}

/// A validated per-case spec cross-checked against its manifest project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseSpec {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub license: String,
    pub source_dir: String,
    pub entrypoint: String,
    pub node_args: Vec<String>,
    pub expected_timeout_ms: u64,
    pub constructs: Vec<String>,
    pub source_files: Vec<String>,
    #[serde(default)]
    pub compiler_args: Vec<String>,
}

impl CaseSpec {
    /// The per-case wall-clock bound applied by the oracle.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.expected_timeout_ms)
    }
}

/// The manifest together with every validated, existence-checked case, aligned
/// with `manifest.projects` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corpus {
    pub manifest: CorpusManifest,
    pub cases: Vec<CaseSpec>,
}

impl Corpus {
    /// Looks up a validated case by id.
    pub fn case(&self, id: &str) -> Option<&CaseSpec> {
        self.cases.iter().find(|case| case.id == id)
    }
}

// ---------------------------------------------------------------------------
// Raw deserialization targets
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: u32,
    node_version: String,
    environment: Vec<String>,
    compare: Vec<String>,
    projects: Vec<RawProject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    id: String,
    repository: String,
    commit: String,
    spec: String,
    entrypoint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    schema: u32,
    id: String,
    repository: String,
    commit: String,
    license: String,
    source_dir: String,
    entrypoint: String,
    node_args: Vec<String>,
    expected_timeout_ms: u64,
    constructs: Vec<String>,
    source_files: Vec<String>,
    #[serde(default)]
    compiler_args: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loading and validation
// ---------------------------------------------------------------------------

/// Parses and validates the corpus manifest without touching per-case specs.
pub fn load_manifest(root: &Path) -> Result<CorpusManifest> {
    let path = root.join(MANIFEST_PATH);
    let raw: RawManifest = parse_toml(&path)?;
    validate_manifest(&path, raw)
}

/// Parses and validates the manifest and every declared case spec, verifying
/// that each declared source directory, entrypoint, and source file exists.
pub fn load_corpus(root: &Path) -> Result<Corpus> {
    let manifest = load_manifest(root)?;
    let manifest_path = root.join(MANIFEST_PATH);
    verify_pinned_case_ids(&manifest_path, &manifest.projects)?;
    verify_exact_layout(root, &manifest)?;
    let mut cases = Vec::with_capacity(manifest.projects.len());
    for project in &manifest.projects {
        let spec_path = root.join(&project.spec);
        let raw: RawSpec = parse_toml(&spec_path)?;
        let spec = validate_spec(&spec_path, raw, project)?;
        verify_case_paths(root, &spec_path, &spec)?;
        cases.push(spec);
    }
    Ok(Corpus { manifest, cases })
}

fn validate_manifest(path: &Path, raw: RawManifest) -> Result<CorpusManifest> {
    require_schema(path, raw.schema)?;
    if raw.node_version != NODE_VERSION {
        return Err(schema_error(
            path,
            format!(
                "manifest node_version must be `{NODE_VERSION}`, found `{}`",
                raw.node_version
            ),
        ));
    }
    require_exact_set(
        path,
        "manifest environment",
        &raw.environment,
        &NORMALIZED_ENV,
    )?;
    require_exact_set(path, "manifest compare", &raw.compare, &COMPARE_KEYS)?;
    if raw.projects.is_empty() {
        return Err(schema_error(path, "manifest declares no projects"));
    }

    let mut ids = BTreeSet::new();
    let mut specs = BTreeSet::new();
    let mut entrypoints = BTreeSet::new();
    let mut projects = Vec::with_capacity(raw.projects.len());
    for raw_project in raw.projects {
        validate_project(path, &raw_project)?;
        if !ids.insert(raw_project.id.clone()) {
            return Err(schema_error(
                path,
                format!("duplicate project id `{}`", raw_project.id),
            ));
        }
        if !specs.insert(raw_project.spec.clone()) {
            return Err(schema_error(
                path,
                format!("duplicate project spec path `{}`", raw_project.spec),
            ));
        }
        if !entrypoints.insert(raw_project.entrypoint.clone()) {
            return Err(schema_error(
                path,
                format!("duplicate project entrypoint `{}`", raw_project.entrypoint),
            ));
        }
        projects.push(ManifestProject {
            id: raw_project.id,
            repository: raw_project.repository,
            commit: raw_project.commit,
            spec: raw_project.spec,
            entrypoint: raw_project.entrypoint,
        });
    }

    Ok(CorpusManifest {
        node_version: raw.node_version,
        environment: raw.environment,
        compare: raw.compare,
        projects,
    })
}

fn validate_project(path: &Path, project: &RawProject) -> Result<()> {
    require_nonempty(path, "project id", &project.id)?;
    require_nonempty(
        path,
        &format!("project `{}` repository", project.id),
        &project.repository,
    )?;
    require_commit(path, &project.id, &project.commit)?;
    require_clean_relative(
        path,
        &format!("project `{}` spec", project.id),
        &project.spec,
    )?;
    require_clean_relative(
        path,
        &format!("project `{}` entrypoint", project.id),
        &project.entrypoint,
    )?;
    require_ts_entrypoint(path, &project.id, &project.entrypoint)
}

fn validate_spec(path: &Path, raw: RawSpec, project: &ManifestProject) -> Result<CaseSpec> {
    require_schema(path, raw.schema)?;
    require_match(path, "id", &raw.id, &project.id)?;
    require_match(path, "repository", &raw.repository, &project.repository)?;
    require_match(path, "commit", &raw.commit, &project.commit)?;
    require_match(path, "entrypoint", &raw.entrypoint, &project.entrypoint)?;
    require_commit(path, &raw.id, &raw.commit)?;
    require_nonempty(path, "license", &raw.license)?;
    require_clean_relative(path, "source_dir", &raw.source_dir)?;
    require_clean_relative(path, "entrypoint", &raw.entrypoint)?;
    require_ts_entrypoint(path, &raw.id, &raw.entrypoint)?;

    require_unique_nonempty(path, "constructs", &raw.constructs)?;

    require_unique_nonempty(path, "source_files", &raw.source_files)?;
    for source_file in &raw.source_files {
        require_clean_relative(path, "source_files entry", source_file)?;
    }

    validate_node_args(path, &raw.node_args)?;
    validate_compiler_args(path, &raw.compiler_args)?;

    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&raw.expected_timeout_ms) {
        return Err(schema_error(
            path,
            format!(
                "expected_timeout_ms must be within {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}, found {}",
                raw.expected_timeout_ms
            ),
        ));
    }

    Ok(CaseSpec {
        id: raw.id,
        repository: raw.repository,
        commit: raw.commit,
        license: raw.license,
        source_dir: raw.source_dir,
        entrypoint: raw.entrypoint,
        node_args: raw.node_args,
        expected_timeout_ms: raw.expected_timeout_ms,
        constructs: raw.constructs,
        source_files: raw.source_files,
        compiler_args: raw.compiler_args,
    })
}

fn verify_pinned_case_ids(path: &Path, projects: &[ManifestProject]) -> Result<()> {
    let manifest_ids: Vec<&str> = projects.iter().map(|project| project.id.as_str()).collect();
    if manifest_ids != PINNED_CASE_IDS {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "{}: manifest project IDs do not match the pinned list: expected {:?}, got {:?}",
                path.display(),
                PINNED_CASE_IDS,
                manifest_ids
            ),
        ));
    }
    let pinned: BTreeSet<&str> = PINNED_CASE_IDS.iter().copied().collect();
    for (name, subset) in [
        ("TASK_106_SYNC_CASE_IDS", TASK_106_SYNC_CASE_IDS.as_slice()),
        ("TASK_107_NODE_CASE_IDS", TASK_107_NODE_CASE_IDS.as_slice()),
    ] {
        if let Some(id) = subset.iter().find(|id| !pinned.contains(*id)) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "{}: {} contains id `{}` not present in PINNED_CASE_IDS",
                    path.display(),
                    name,
                    *id
                ),
            ));
        }
    }
    Ok(())
}

fn verify_exact_layout(root: &Path, manifest: &CorpusManifest) -> Result<()> {
    let expected_specs = manifest
        .projects
        .iter()
        .map(|project| project.spec.clone())
        .collect::<BTreeSet<_>>();
    let expected_cases = manifest
        .projects
        .iter()
        .map(|project| project.entrypoint.clone())
        .collect::<BTreeSet<_>>();

    require_same_paths(
        root,
        "corpus spec files",
        &expected_specs,
        &directory_entries(root, "corpus/specs")?,
    )?;
    require_same_paths(
        root,
        "corpus case entrypoints",
        &expected_cases,
        &directory_entries(root, "corpus/cases")?,
    )
}

fn directory_entries(root: &Path, relative: &str) -> Result<BTreeSet<String>> {
    let directory = root.join(relative);
    let mut paths = BTreeSet::new();
    let entries = fs::read_dir(&directory).map_err(|error| io_error(&directory, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(&directory, &error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| io_error(&entry.path(), &error))?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(schema_error(
                &entry.path(),
                "expected a regular corpus file",
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| schema_error(&entry.path(), "corpus entry name must be valid UTF-8"))?;
        paths.insert(format!("{relative}/{name}"));
    }
    Ok(paths)
}

fn require_same_paths(
    root: &Path,
    kind: &str,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    let missing = expected
        .difference(actual)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let extra = actual
        .difference(expected)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    Err(VerificationError::new(
        ErrorCode::SetMismatch,
        format!(
            "{}: {kind}: missing [{missing}]; extra [{extra}]",
            root.display()
        ),
    ))
}

fn verify_case_paths(root: &Path, spec_path: &Path, spec: &CaseSpec) -> Result<()> {
    require_dir(root, spec_path, &spec.source_dir)?;
    require_file(root, spec_path, &spec.entrypoint)?;
    for source_file in &spec.source_files {
        require_file(root, spec_path, source_file)?;
    }
    Ok(())
}

fn validate_node_args(path: &Path, args: &[String]) -> Result<()> {
    // node_args are forwarded verbatim to the interpreter; an empty argument is
    // never meaningful and signals a malformed spec.
    if args.iter().any(|arg| arg.is_empty()) {
        return Err(schema_error(
            path,
            "node_args must not contain empty entries",
        ));
    }
    Ok(())
}

fn validate_compiler_args(path: &Path, args: &[String]) -> Result<()> {
    for argument in args {
        let trimmed = argument.trim();
        if trimmed.is_empty() {
            return Err(schema_error(
                path,
                "compiler_args must not contain empty or whitespace-only entries",
            ));
        }
        let name = trimmed.split_once('=').map_or(trimmed, |(name, _)| name);
        if HARNESS_OWNED_ARGS.contains(&name) {
            return Err(schema_error(
                path,
                format!("compiler_args contains harness-owned argument `{argument}`"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Node oracle
// ---------------------------------------------------------------------------

/// Bounds applied to a single oracle invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// The captured result of running a case under the Node oracle.
///
/// The parity key is `(stdout, exit_code)` per the manifest's `compare` set.
/// `stderr` is retained as executable evidence and is never part of the parity key.
/// AOT compilation diagnostics are retained separately for diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleOutcome {
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    pub compile_stderr: Vec<u8>,
    pub compile_stderr_truncated: bool,
}

impl OracleOutcome {
    /// The differential parity key: raw stdout bytes and the exact exit code.
    pub fn parity_key(&self) -> (&[u8], Option<i32>) {
        (&self.stdout, self.exit_code)
    }

    /// Whether the parity key is trustworthy: the process ran to completion
    /// within its bound and its stdout was captured whole.  A timed-out run
    /// (exit code lost to a kill signal) or a truncated stdout only exposes a
    /// prefix, so its parity key must never be treated as authoritative.
    pub fn is_reliable(&self) -> bool {
        !self.timed_out && !self.stdout_truncated
    }

    /// Whether two outcomes agree on the parity key (stdout + exit code).
    ///
    /// Returns `false` unless *both* outcomes are [`is_reliable`]: a kill or a
    /// truncation is a non-answer, never a match, so the bounds the oracle
    /// enforces can never masquerade as agreement.
    ///
    /// [`is_reliable`]: OracleOutcome::is_reliable
    pub fn parity_matches(&self, other: &OracleOutcome) -> bool {
        self.is_reliable()
            && other.is_reliable()
            && self.exit_code == other.exit_code
            && self.stdout == other.stdout
    }
}

/// A pinned Node interpreter bound to a repository root.
#[derive(Debug, Clone)]
pub struct NodeOracle {
    node: PathBuf,
    root: PathBuf,
    environment: Vec<(String, String)>,
    max_output_bytes: usize,
}

impl NodeOracle {
    /// Builds an oracle from an explicit interpreter path, verifying it reports
    /// exactly [`NODE_VERSION`].
    pub fn new(root: &Path, node: &Path) -> Result<Self> {
        verify_node_version(node)?;
        Ok(Self {
            node: node.to_path_buf(),
            root: root.to_path_buf(),
            environment: normalized_env(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        })
    }

    /// Locates `node` on `PATH`, then builds and version-verifies an oracle.
    pub fn discover(root: &Path) -> Result<Self> {
        let node = locate_node()?;
        Self::new(root, &node)
    }

    /// Overrides the captured-output ceiling (default 1 MiB per stream).
    pub fn with_max_output_bytes(mut self, cap: usize) -> Self {
        self.max_output_bytes = cap;
        self
    }

    /// The pinned interpreter path.
    pub fn node(&self) -> &Path {
        &self.node
    }

    /// Runs a validated case entrypoint and returns its bounded outcome.
    pub fn run_case(&self, spec: &CaseSpec) -> Result<OracleOutcome> {
        let entrypoint_root = self.root.join(MANIFEST_PATH);
        require_clean_relative(&entrypoint_root, "entrypoint", &spec.entrypoint)
            .map_err(|error| corpus_stage_error(CorpusStage::Resolve, error))?;
        let mut args: Vec<OsString> = Vec::with_capacity(spec.node_args.len() + 1);
        for arg in &spec.node_args {
            args.push(OsString::from(arg));
        }
        args.push(OsString::from(&spec.entrypoint));
        let limits = OracleLimits {
            timeout: spec.timeout(),
            max_output_bytes: self.max_output_bytes,
        };
        run_node(&self.node, &self.root, &self.environment, &args, &limits)
            .map_err(|error| corpus_stage_error(CorpusStage::Spawn, error))
    }
}

/// BamTS execution paths covered by the differential harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Interpreter,
    Jit,
    Aot,
}

impl ExecutionMode {
    pub const ALL: [Self; 3] = [Self::Interpreter, Self::Jit, Self::Aot];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
            Self::Aot => "aot",
        }
    }
}

/// The first corpus-harness stage at which execution could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CorpusStage {
    Load,
    Resolve,
    Check,
    Lower,
    Verify,
    Instantiate,
    Evaluate,
    Link,
    Spawn,
    Compare,
}

impl CorpusStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Resolve => "resolve",
            Self::Check => "check",
            Self::Lower => "lower",
            Self::Verify => "verify",
            Self::Instantiate => "instantiate",
            Self::Evaluate => "evaluate",
            Self::Link => "link",
            Self::Spawn => "spawn",
            Self::Compare => "compare",
        }
    }
}

/// A classified execution failure with bounded, directly-observed evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusFailure {
    pub stage: CorpusStage,
    pub evidence: String,
}

impl CorpusFailure {
    #[must_use]
    pub fn from_driver_error(error: &driver::DriverError) -> Self {
        let stage = match error {
            driver::DriverError::ReadSource { .. } | driver::DriverError::ProgramLoad(_) => {
                CorpusStage::Load
            }
            driver::DriverError::UnsupportedSourceExtension { .. }
            | driver::DriverError::Usage(_)
            | driver::DriverError::LintConfig { .. }
            | driver::DriverError::ProjectConfig { .. }
            | driver::DriverError::NonUnicodeEnvironmentName { .. }
            | driver::DriverError::NonUnicodeEnvironmentValue { .. }
            | driver::DriverError::MissingEntrypoint
            | driver::DriverError::MultipleCompileInputs
            | driver::DriverError::UnsupportedCompileTarget(_)
            | driver::DriverError::UnsupportedOutputOption(_)
            | driver::DriverError::UnexpectedResolution(_) => CorpusStage::Resolve,
            driver::DriverError::Diagnostics { .. } => CorpusStage::Check,
            driver::DriverError::Lower(error) => program_lower_stage(error),
            driver::DriverError::Jit(error) => jit_stage(error),
            driver::DriverError::Native(error) => native_stage(error),
            driver::DriverError::Aot(error) => aot_stage(error),
            driver::DriverError::CreateDirectory { .. }
            | driver::DriverError::UnsafeFallbackCacheRoot { .. }
            | driver::DriverError::CacheArchive { .. }
            | driver::DriverError::WriteObject { .. }
            | driver::DriverError::ToolchainMissing { .. }
            | driver::DriverError::ToolchainProbe { .. }
            | driver::DriverError::ToolchainRejected { .. }
            | driver::DriverError::LinkStart { .. }
            | driver::DriverError::LinkFailed { .. }
            | driver::DriverError::PublishExecutable { .. }
            | driver::DriverError::Cancelled
            | driver::DriverError::CrossTargetLink { .. } => CorpusStage::Link,
        };
        Self {
            stage,
            evidence: driver_error_evidence(error),
        }
    }

    /// Classifies a facade failure observed by the in-process interpreter.
    /// Compile and load failures keep their own stages; they are never
    /// reported as evaluation failures.
    #[must_use]
    pub fn from_facade_error(error: &bamts::Error) -> Self {
        let stage = match error {
            bamts::Error::ReadConfig { .. } | bamts::Error::ProgramLoad(_) => CorpusStage::Load,
            bamts::Error::ProjectConfig { .. } => CorpusStage::Resolve,
            bamts::Error::Diagnostics { .. } => CorpusStage::Check,
            bamts::Error::Lower(error) => program_lower_stage(error),
            bamts::Error::Runtime(_) => CorpusStage::Evaluate,
            bamts::Error::Aot(error) => aot_stage(error),
            _ => CorpusStage::Load,
        };
        Self {
            stage,
            evidence: facade_error_evidence(error),
        }
    }
}

impl std::fmt::Display for CorpusFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "stage={}: {}",
            self.stage.as_str(),
            self.evidence
        )
    }
}

const FAILURE_EVIDENCE_BYTES: usize = 512;

fn program_lower_stage(error: &bamts_compiler::program::ProgramLowerError) -> CorpusStage {
    match &error.kind {
        bamts_compiler::program::ProgramLowerErrorKind::Lower(error) => lower_stage(error),
        bamts_compiler::program::ProgramLowerErrorKind::Link(_) => CorpusStage::Verify,
        _ => CorpusStage::Lower,
    }
}

fn lower_stage(error: &bamts_compiler::lower::LowerError) -> CorpusStage {
    if matches!(
        &error.kind,
        bamts_compiler::lower::LowerErrorKind::Verify(_)
    ) {
        CorpusStage::Verify
    } else {
        CorpusStage::Lower
    }
}

fn jit_stage(error: &bamts_codegen::JitError) -> CorpusStage {
    match error {
        bamts_codegen::JitError::Lower(_) => CorpusStage::Lower,
        bamts_codegen::JitError::InvalidLoweredModule(_)
        | bamts_codegen::JitError::Module(_)
        | bamts_codegen::JitError::Cancelled
        | bamts_codegen::JitError::UnknownHelper { .. } => CorpusStage::Instantiate,
    }
}

fn aot_stage(error: &bamts_codegen::AotError) -> CorpusStage {
    match error {
        bamts_codegen::AotError::Lower(_) => CorpusStage::Lower,
        bamts_codegen::AotError::TargetLookup(_)
        | bamts_codegen::AotError::TargetBuild(_)
        | bamts_codegen::AotError::TargetEndianness(_)
        | bamts_codegen::AotError::InvalidLoweredModule(_)
        | bamts_codegen::AotError::Module(_)
        | bamts_codegen::AotError::Cancelled
        | bamts_codegen::AotError::Emit(_) => CorpusStage::Instantiate,
    }
}

fn native_stage(error: &bamts_runtime::NativeError) -> CorpusStage {
    match error {
        bamts_runtime::NativeError::Runtime(_)
        | bamts_runtime::NativeError::FatalTrap { .. }
        | bamts_runtime::NativeError::Cancelled => CorpusStage::Evaluate,
        bamts_runtime::NativeError::Abi(_) | bamts_runtime::NativeError::ProgramMismatch => {
            CorpusStage::Link
        }
    }
}

fn driver_error_evidence(error: &driver::DriverError) -> String {
    match error {
        driver::DriverError::Diagnostics { rendered, .. } => {
            let code = first_diagnostic_code(rendered);
            match code {
                Some(code) => format!("diagnostic={code}; rendered={}", bounded_text(rendered)),
                None => format!("rendered={}", bounded_text(rendered)),
            }
        }
        driver::DriverError::Native(bamts_runtime::NativeError::Runtime(error)) => format!(
            "runtime function={} pc={} opcode={:?}; error={}",
            error.function.get(),
            error.pc.get(),
            error.source.instruction,
            bounded_text(&error.to_string())
        ),
        _ => bounded_text(&error.to_string()),
    }
}

fn facade_error_evidence(error: &bamts::Error) -> String {
    match error {
        bamts::Error::Diagnostics { diagnostics } => match diagnostics.first() {
            Some(first) => format!(
                "diagnostic={}; {}",
                first.code(),
                bounded_text(&error.to_string())
            ),
            None => bounded_text(&error.to_string()),
        },
        bamts::Error::Runtime(error) => format!(
            "runtime function={} pc={} opcode={:?}; error={}",
            error.function.get(),
            error.pc.get(),
            error.source.instruction,
            bounded_text(&error.to_string())
        ),
        _ => bounded_text(&error.to_string()),
    }
}

fn first_diagnostic_code(rendered: &str) -> Option<&str> {
    let bytes = rendered.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    for start in 0..=bytes.len() - 10 {
        let candidate = &bytes[start..start + 10];
        if candidate.starts_with(b"BAMTS-")
            && candidate[6].is_ascii_uppercase()
            && candidate[7..].iter().all(u8::is_ascii_digit)
        {
            return rendered.get(start..start + 10);
        }
    }
    None
}

fn bounded_text(text: &str) -> String {
    let mut evidence = text
        .bytes()
        .take(FAILURE_EVIDENCE_BYTES)
        .flat_map(|byte| byte.escape_ascii())
        .map(char::from)
        .collect::<String>();
    if text.len() > FAILURE_EVIDENCE_BYTES {
        evidence.push_str("...");
    }
    evidence
}

/// Runs validated corpus cases through the public `bamts_cli` driver.
///
/// # Preconditions
///
/// The repository manifest must already have been validated by [`load_corpus`].
/// Every execution mode re-executes [`std::env::current_exe`] as a libtest
/// worker, so the current executable must be a libtest binary containing an
/// `#[ignore]` test named `corpus_differential_worker` that is selected with
/// `--exact corpus_differential_worker --ignored --nocapture` and calls
/// [`run_corpus_worker_from_env`].
#[derive(Debug, Clone)]
pub struct BamtsRunner {
    root: PathBuf,
    max_output_bytes: usize,
}

impl BamtsRunner {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Overrides the captured-output ceiling (default 1 MiB per stream).
    pub fn with_max_output_bytes(mut self, cap: usize) -> Self {
        self.max_output_bytes = cap;
        self
    }

    /// Runs one validated case through Interpreter, JIT, or AOT under the
    /// case bound.
    pub fn run_case(&self, spec: &CaseSpec, mode: ExecutionMode) -> Result<OracleOutcome> {
        let manifest = self.root.join(MANIFEST_PATH);
        require_clean_relative(&manifest, "entrypoint", &spec.entrypoint)
            .map_err(|error| corpus_stage_error(CorpusStage::Resolve, error))?;
        match mode {
            ExecutionMode::Interpreter | ExecutionMode::Jit => {
                self.run_in_process_worker(spec, mode)
            }
            ExecutionMode::Aot => self.run_aot(spec),
        }
    }

    fn run_in_process_worker(&self, spec: &CaseSpec, mode: ExecutionMode) -> Result<OracleOutcome> {
        let operation = match mode {
            ExecutionMode::Interpreter => WorkerOperation::Interpreter,
            ExecutionMode::Jit => WorkerOperation::Jit,
            ExecutionMode::Aot => unreachable!("AOT uses run_aot"),
        };
        let started = Instant::now();
        let artifacts = ArtifactDirectory::create(&self.root, spec, mode)
            .map_err(|error| corpus_stage_error(CorpusStage::Spawn, error))?;
        let Some(budget) = remaining_case_budget(spec.timeout(), started.elapsed()) else {
            return Ok(timeout_outcome(Vec::new(), self.max_output_bytes));
        };
        let request = WorkerRequest {
            root: self.root.clone(),
            spec: spec.clone(),
            operation,
            max_output_bytes: self.max_output_bytes,
            executable: None,
        };
        match run_worker(&artifacts, &request, budget)? {
            WorkerRun::TimedOut(outcome) => Ok(outcome),
            WorkerRun::Completed(WorkerResponse::Outcome(outcome)) => Ok(outcome),
            WorkerRun::Completed(WorkerResponse::Failure(failure)) => {
                Err(mode_failure(spec, mode, failure))
            }
            WorkerRun::Completed(WorkerResponse::Compile { .. }) => Err(VerificationError::new(
                ErrorCode::ToolFailed,
                format!(
                    "corpus {} worker returned an AOT compile response",
                    mode.as_str()
                ),
            )),
        }
    }

    fn run_aot(&self, spec: &CaseSpec) -> Result<OracleOutcome> {
        let started = Instant::now();
        let artifacts = ArtifactDirectory::create(&self.root, spec, ExecutionMode::Aot)
            .map_err(|error| corpus_stage_error(CorpusStage::Link, error))?;
        let executable = artifacts.executable(spec);
        let Some(compile_budget) = remaining_case_budget(spec.timeout(), started.elapsed()) else {
            return Ok(timeout_outcome(Vec::new(), self.max_output_bytes));
        };
        let request = WorkerRequest {
            root: self.root.clone(),
            spec: spec.clone(),
            operation: WorkerOperation::AotCompile,
            max_output_bytes: self.max_output_bytes,
            executable: Some(executable.clone()),
        };
        let (compile_stderr, compile_stderr_truncated) =
            match run_worker(&artifacts, &request, compile_budget)? {
                WorkerRun::TimedOut(outcome) => return Ok(outcome),
                WorkerRun::Completed(WorkerResponse::Compile {
                    stderr,
                    stderr_truncated,
                }) => (stderr, stderr_truncated),
                WorkerRun::Completed(WorkerResponse::Failure(failure)) => {
                    return Err(mode_failure(spec, ExecutionMode::Aot, failure));
                }
                WorkerRun::Completed(WorkerResponse::Outcome(_)) => {
                    return Err(VerificationError::new(
                        ErrorCode::ToolFailed,
                        "corpus AOT worker returned a JIT execution response",
                    ));
                }
            };
        let Some(execution_budget) = remaining_case_budget(spec.timeout(), started.elapsed())
        else {
            return Ok(with_aot_compile_evidence(
                timeout_outcome(Vec::new(), self.max_output_bytes),
                compile_stderr,
                compile_stderr_truncated,
                self.max_output_bytes,
            ));
        };
        let outcome = run_process(
            "BamTS AOT executable",
            &executable,
            &self.root,
            &normalized_env(),
            &[],
            &aot_execution_limits(execution_budget, self.max_output_bytes),
        )
        .map_err(|error| corpus_stage_error(CorpusStage::Spawn, error))?;
        Ok(with_aot_compile_evidence(
            outcome,
            compile_stderr,
            compile_stderr_truncated,
            self.max_output_bytes,
        ))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerRequest {
    root: PathBuf,
    spec: CaseSpec,
    operation: WorkerOperation,
    max_output_bytes: usize,
    executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WorkerOperation {
    Interpreter,
    Jit,
    AotCompile,
}

#[derive(Debug, Serialize, Deserialize)]
enum WorkerResponse {
    Outcome(OracleOutcome),
    Compile {
        stderr: Vec<u8>,
        stderr_truncated: bool,
    },
    Failure(CorpusFailure),
}

enum WorkerRun {
    TimedOut(OracleOutcome),
    Completed(WorkerResponse),
}

/// Executes the killable corpus worker selected by the integration harness.
///
/// The worker communicates only through request/response files so test-runner
/// output can never contaminate the differential parity key.
pub fn run_corpus_worker_from_env() -> Result<()> {
    let request_path = env::var_os(CORPUS_WORKER_REQUEST).ok_or_else(|| {
        VerificationError::new(ErrorCode::Usage, "corpus worker request path is missing")
    })?;
    let request_path = PathBuf::from(request_path);
    let response_path = request_path.with_extension("response.json");
    let request: WorkerRequest = serde_json::from_slice(
        &fs::read(&request_path).map_err(|error| io_error(&request_path, &error))?,
    )
    .map_err(|error| json_error(&request_path, error))?;
    let response = execute_worker_request(&request)?;
    let encoded =
        serde_json::to_vec(&response).map_err(|error| json_error(&response_path, error))?;
    fs::write(&response_path, encoded).map_err(|error| io_error(&response_path, &error))
}

fn execute_worker_request(request: &WorkerRequest) -> Result<WorkerResponse> {
    match request.operation {
        WorkerOperation::Interpreter => execute_interpreter_request(request),
        WorkerOperation::Jit | WorkerOperation::AotCompile => execute_driver_request(request),
    }
}

/// JIT/AOT worker: compiles and executes via the public driver in-process.
fn execute_driver_request(request: &WorkerRequest) -> Result<WorkerResponse> {
    let (mode, target, output) = match request.operation {
        WorkerOperation::Jit => (Mode::Run, ExecutionTarget::Jit, None),
        WorkerOperation::AotCompile => (
            Mode::Compile,
            ExecutionTarget::Aot,
            request.executable.as_deref(),
        ),
        WorkerOperation::Interpreter => {
            unreachable!("interpreter requests are handled by execute_interpreter_request")
        }
    };
    let args = cli_args(
        mode,
        target,
        request.root.join(&request.spec.entrypoint),
        output,
        case_requires_javascript_compatibility(&request.spec),
        &request.spec.compiler_args,
    )?;
    match driver::execute(&args) {
        Ok(outcome) => match request.operation {
            WorkerOperation::Jit => Ok(WorkerResponse::Outcome(driver_outcome(
                outcome,
                false,
                request.max_output_bytes,
            ))),
            WorkerOperation::AotCompile => {
                let (stderr, stderr_truncated) =
                    bounded_output(outcome.stderr, request.max_output_bytes);
                Ok(WorkerResponse::Compile {
                    stderr,
                    stderr_truncated,
                })
            }
            WorkerOperation::Interpreter => {
                unreachable!("interpreter requests are handled by execute_interpreter_request")
            }
        },
        Err(error) if is_unhandled_driver_throw(&error) => {
            Ok(WorkerResponse::Outcome(process_rejection_outcome(
                Vec::new(),
                error.to_string().into_bytes(),
                request.max_output_bytes,
            )))
        }
        Err(error) => Ok(WorkerResponse::Failure(CorpusFailure::from_driver_error(
            &error,
        ))),
    }
}

/// Interpreter worker: compiles to bytecode and runs it in-process through
/// `bamts_runtime::run` against a Node host.  Fuel bounds interpreted loops,
/// while the enclosing worker process is wall-clock killable by `run_worker`.
fn execute_interpreter_request(request: &WorkerRequest) -> Result<WorkerResponse> {
    // Compilation happens inside the worker, so it spends the case's wall-clock
    // budget; the interpreter run below gets only what is left.
    let started = Instant::now();
    let entrypoint = request.root.join(&request.spec.entrypoint);
    // `compile_program` runs only the frontend, lint, and lowering pipeline —
    // it never reads `args.target`, so ExecutionTarget is irrelevant to the
    // bytecode it produces.  There is no ExecutionTarget::Interpreter variant;
    // the actual interpreter execution happens below via `bamts_runtime::run`,
    // which interprets the bytecode directly and is distinct from JIT mode's
    // `bamts_codegen::compile_jit` + native `run_linked_program`.
    let args = cli_args(
        Mode::Run,
        ExecutionTarget::Jit,
        entrypoint.clone(),
        None,
        case_requires_javascript_compatibility(&request.spec),
        &request.spec.compiler_args,
    )?;
    let executable = match driver::compile_program(&args) {
        Ok(executable) => executable,
        Err(error) => {
            return Ok(WorkerResponse::Failure(CorpusFailure::from_driver_error(
                &error,
            )));
        }
    };
    let remaining = request.spec.timeout().saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Ok(WorkerResponse::Outcome(timeout_outcome(
            Vec::new(),
            request.max_output_bytes,
        )));
    }

    let mut host = bamts_node::NodeHost::new();
    host.set_script_compiler(Box::new(bamts_node::ScriptCompiler));
    host.set_argv(["bamts".to_owned(), entrypoint.display().to_string()]);
    let limits = interpreter_limits(remaining);
    let outcome = match bamts_runtime::run(executable.wire(), &mut host, &limits) {
        Ok(outcome) => outcome,
        Err(error)
            if matches!(
                &error.kind,
                bamts_runtime::RuntimeErrorKind::FuelExhausted { .. }
            ) =>
        {
            return Ok(WorkerResponse::Outcome(timeout_outcome(
                host.stderr().to_vec(),
                request.max_output_bytes,
            )));
        }
        Err(error)
            if matches!(
                &error.kind,
                bamts_runtime::RuntimeErrorKind::UncaughtThrow { .. }
            ) =>
        {
            return Ok(WorkerResponse::Outcome(process_rejection_outcome(
                host.stdout().to_vec(),
                host.stderr().to_vec(),
                request.max_output_bytes,
            )));
        }
        Err(error) => {
            return Ok(WorkerResponse::Failure(CorpusFailure::from_facade_error(
                &bamts::Error::from(error),
            )));
        }
    };
    let mut stdout = host.stdout().to_vec();
    stdout.extend_from_slice(&outcome.stdout);
    let exit_code = host.completion_exit_code(outcome.exit_code);
    Ok(WorkerResponse::Outcome(driver_outcome(
        driver::CommandOutcome {
            stdout,
            stderr: host.stderr().to_vec(),
            exit_code,
            ..driver::CommandOutcome::default()
        },
        started.elapsed() >= request.spec.timeout(),
        request.max_output_bytes,
    )))
}

fn run_worker(
    artifacts: &ArtifactDirectory,
    request: &WorkerRequest,
    budget: Duration,
) -> Result<WorkerRun> {
    let request_path = artifacts.0.join("request.json");
    let response_path = request_path.with_extension("response.json");
    let encoded = serde_json::to_vec(request).map_err(|error| json_error(&request_path, error))?;
    fs::write(&request_path, encoded).map_err(|error| io_error(&request_path, &error))?;
    let current_exe = env::current_exe().map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot resolve corpus worker executable: {error}"),
        )
    })?;
    let mut environment = worker_env(request.operation);
    environment.push((
        CORPUS_WORKER_REQUEST.to_owned(),
        request_path.to_string_lossy().into_owned(),
    ));
    let args = [
        OsString::from("--exact"),
        OsString::from(CORPUS_WORKER_TEST),
        OsString::from("--ignored"),
        OsString::from("--nocapture"),
    ];
    let process = run_process(
        "BamTS corpus worker",
        &current_exe,
        &request.root,
        &environment,
        &args,
        &OracleLimits {
            timeout: budget,
            max_output_bytes: request.max_output_bytes,
        },
    )?;
    if process.timed_out {
        return Ok(WorkerRun::TimedOut(timeout_outcome(
            process.stderr,
            request.max_output_bytes,
        )));
    }
    if process.exit_code != Some(0) {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "BamTS corpus worker exited with {:?}: stdout={}; stderr={}",
                process.exit_code,
                bounded_text(&String::from_utf8_lossy(&process.stdout)),
                bounded_text(&String::from_utf8_lossy(&process.stderr))
            ),
        ));
    }
    if !response_path.exists() {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "BamTS corpus worker `{}` (test `{}`, entry point `run_corpus_worker_from_env`) exited with code 0 but wrote no response; captured stdout: {}; stderr: {}",
                current_exe.display(),
                CORPUS_WORKER_TEST,
                bounded_text(&String::from_utf8_lossy(&process.stdout)),
                bounded_text(&String::from_utf8_lossy(&process.stderr))
            ),
        ));
    }
    let response: WorkerResponse = serde_json::from_slice(
        &fs::read(&response_path).map_err(|error| io_error(&response_path, &error))?,
    )
    .map_err(|error| json_error(&response_path, error))?;
    Ok(WorkerRun::Completed(response))
}

/// Builds the worker process environment, reusing the canonical
/// [`normalized_env`] as the base.  Only the AOT compile step needs a
/// discoverable toolchain (the linker); the JIT and interpreter workers
/// execute the case program in-process and must observe the same
/// normalized environment as the Node oracle and the AOT executable.
fn worker_env(operation: WorkerOperation) -> Vec<(String, String)> {
    let mut environment = normalized_env();
    if matches!(operation, WorkerOperation::AotCompile)
        && let Some(path) = env::var_os("PATH")
    {
        environment.push(("PATH".to_owned(), path.to_string_lossy().into_owned()));
    }
    environment
}

fn interpreter_limits(budget: Duration) -> Limits {
    let milliseconds = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX).max(1);
    Limits {
        fuel: milliseconds.saturating_mul(INTERPRETER_FUEL_PER_MILLISECOND),
        ..Limits::default()
    }
}

fn is_unhandled_driver_throw(error: &driver::DriverError) -> bool {
    matches!(
        error,
        driver::DriverError::Native(bamts_runtime::NativeError::Runtime(
            bamts_runtime::RuntimeError {
                kind: bamts_runtime::RuntimeErrorKind::UncaughtThrow { .. },
                ..
            }
        ))
    )
}

fn process_rejection_outcome(stdout: Vec<u8>, stderr: Vec<u8>, cap: usize) -> OracleOutcome {
    driver_outcome(
        driver::CommandOutcome {
            stdout,
            stderr,
            exit_code: 1,
            ..driver::CommandOutcome::default()
        },
        false,
        cap,
    )
}

fn timeout_outcome(stderr: Vec<u8>, cap: usize) -> OracleOutcome {
    let (stderr, stderr_truncated) = bounded_output(stderr, cap);
    OracleOutcome {
        timed_out: true,
        exit_code: None,
        signal: None,
        stdout: Vec::new(),
        stdout_truncated: false,
        stderr,
        stderr_truncated,
        compile_stderr: Vec::new(),
        compile_stderr_truncated: false,
    }
}

fn json_error(path: &Path, error: serde_json::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Json, format!("{}: {error}", path.display()))
}

fn remaining_case_budget(timeout: Duration, elapsed: Duration) -> Option<Duration> {
    if elapsed < timeout {
        Some(timeout - elapsed)
    } else {
        None
    }
}

fn aot_execution_limits(timeout: Duration, max_output_bytes: usize) -> OracleLimits {
    OracleLimits {
        timeout,
        max_output_bytes,
    }
}

fn with_aot_compile_evidence(
    mut runtime: OracleOutcome,
    compile_stderr: Vec<u8>,
    compile_stderr_truncated: bool,
    max_output_bytes: usize,
) -> OracleOutcome {
    let (compile_stderr, truncated_here) = bounded_output(compile_stderr, max_output_bytes);
    runtime.compile_stderr = compile_stderr;
    runtime.compile_stderr_truncated = compile_stderr_truncated || truncated_here;
    runtime
}

fn case_requires_javascript_compatibility(spec: &CaseSpec) -> bool {
    std::iter::once(spec.entrypoint.as_str())
        .chain(spec.source_files.iter().map(String::as_str))
        .any(|path| {
            [".js", ".jsx", ".mjs", ".cjs"]
                .iter()
                .any(|suffix| path.ends_with(suffix))
        })
}

fn cli_arg_strings(
    mode: Mode,
    target: ExecutionTarget,
    entrypoint: PathBuf,
    output: Option<&Path>,
    javascript_compatibility: bool,
    compiler_args: &[String],
) -> Vec<String> {
    let mut raw = vec![
        harness_arg(&mode.to_string()),
        entrypoint.to_string_lossy().into_owned(),
        harness_arg("--target"),
        harness_arg(&target.to_string()),
    ];
    if javascript_compatibility {
        raw.push(harness_arg("--js-compat"));
    }
    if let Some(path) = output {
        raw.push(harness_arg("--output"));
        raw.push(path.to_string_lossy().into_owned());
    }
    raw.extend(compiler_args.iter().cloned());
    raw
}

pub(crate) fn cli_args(
    mode: Mode,
    target: ExecutionTarget,
    entrypoint: PathBuf,
    output: Option<&Path>,
    javascript_compatibility: bool,
    compiler_args: &[String],
) -> Result<CliArgs> {
    parse_args(cli_arg_strings(
        mode,
        target,
        entrypoint,
        output,
        javascript_compatibility,
        compiler_args,
    ))
    .map_err(|error| {
        VerificationError::new(
            ErrorCode::Usage,
            format!("cannot construct corpus CLI invocation: {error}"),
        )
    })
}

fn harness_arg(name: &str) -> String {
    debug_assert!(
        HARNESS_OWNED_ARGS.contains(&name),
        "corpus CLI builder may only emit harness-owned arguments"
    );
    name.to_owned()
}

fn driver_outcome(outcome: driver::CommandOutcome, timed_out: bool, cap: usize) -> OracleOutcome {
    let (stdout, stdout_truncated) = bounded_output(outcome.stdout, cap);
    let (stderr, stderr_truncated) = bounded_output(outcome.stderr, cap);
    OracleOutcome {
        timed_out,
        exit_code: Some(outcome.exit_code),
        signal: None,
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        compile_stderr: Vec::new(),
        compile_stderr_truncated: false,
    }
}

pub(crate) fn bounded_output(mut output: Vec<u8>, cap: usize) -> (Vec<u8>, bool) {
    let truncated = output.len() > cap;
    output.truncate(cap);
    (output, truncated)
}

static NEXT_ARTIFACT_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct ArtifactDirectory(pub(crate) PathBuf);

impl ArtifactDirectory {
    pub(crate) fn create(root: &Path, spec: &CaseSpec, mode: ExecutionMode) -> Result<Self> {
        let path = root
            .join("target/corpus-differential")
            .join(&spec.id)
            .join(mode.as_str())
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_ARTIFACT_DIRECTORY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|error| io_error(&path, &error))?;
        }
        fs::create_dir_all(&path).map_err(|error| io_error(&path, &error))?;
        Ok(Self(path))
    }

    pub(crate) fn executable(&self, spec: &CaseSpec) -> PathBuf {
        self.0
            .join(format!("{}{}", spec.id, env::consts::EXE_SUFFIX))
    }
}

impl Drop for ArtifactDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode_failure(spec: &CaseSpec, mode: ExecutionMode, failure: CorpusFailure) -> VerificationError {
    VerificationError::new(
        ErrorCode::ToolFailed,
        format!(
            "case `{}` failed in {} mode: {failure}",
            spec.id,
            mode.as_str()
        ),
    )
}

fn corpus_stage_error(stage: CorpusStage, error: VerificationError) -> VerificationError {
    VerificationError::new(
        error.code(),
        format!(
            "stage={}: {}",
            stage.as_str(),
            bounded_text(&error.to_string())
        ),
    )
}

/// Verifies that `node` runs and reports exactly [`NODE_VERSION_OUTPUT`].
pub fn verify_node_version(node: &Path) -> Result<()> {
    let limits = OracleLimits {
        timeout: NODE_VERSION_TIMEOUT,
        max_output_bytes: NODE_VERSION_OUTPUT_CAP,
    };
    let outcome = run_node(
        node,
        &env::temp_dir(),
        &normalized_env(),
        &[OsString::from("--version")],
        &limits,
    )?;
    if outcome.timed_out {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("`{} --version` timed out", node.display()),
        ));
    }
    if outcome.exit_code != Some(0) {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "`{} --version` exited with {:?}",
                node.display(),
                outcome.exit_code
            ),
        ));
    }
    let reported = String::from_utf8_lossy(&outcome.stdout);
    let reported = reported.trim();
    if reported != NODE_VERSION_OUTPUT {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("Node oracle reported `{reported}`, expected `{NODE_VERSION_OUTPUT}`"),
        ));
    }
    Ok(())
}

fn run_node(
    node: &Path,
    cwd: &Path,
    environment: &[(String, String)],
    args: &[OsString],
    limits: &OracleLimits,
) -> Result<OracleOutcome> {
    run_process("Node oracle", node, cwd, environment, args, limits)
}

pub(crate) fn run_process(
    label: &str,
    program: &Path,
    cwd: &Path,
    environment: &[(String, String)],
    args: &[OsString],
    limits: &OracleLimits,
) -> Result<OracleOutcome> {
    let mut command = Command::new(program);
    command.env_clear();
    for (key, value) in environment {
        command.env(key, value);
    }
    command
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Place the worker in a new process group so `libc::killpg` can
        // terminate the whole tree on timeout, not just the immediate child.
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(label, program, &error))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| pipe_error(label, "stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| pipe_error(label, "stderr"))?;

    let cap = limits.max_output_bytes;
    let stdout_handle = drain_stream(stdout, cap);
    let stderr_handle = drain_stream(stderr, cap);

    let start = Instant::now();
    let mut timed_out = false;
    let mut termination_error = None;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| wait_error(label, error))? {
            break status;
        }
        if start.elapsed() >= limits.timeout {
            termination_error = terminate_process_group(&mut child, label).err();
            timed_out = true;
            break child.wait().map_err(|error| wait_error(label, error))?;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let (stdout, stdout_truncated) = stdout_handle
        .join()
        .map_err(|_| thread_error(label, "stdout"))?;
    let (stderr, stderr_truncated) = stderr_handle
        .join()
        .map_err(|_| thread_error(label, "stderr"))?;
    if let Some(error) = termination_error {
        return Err(error);
    }

    Ok(OracleOutcome {
        timed_out,
        exit_code: status.code(),
        signal: termination_signal(&status),
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        compile_stderr: Vec::new(),
        compile_stderr_truncated: false,
    })
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn terminate_process_group(child: &mut std::process::Child, label: &str) -> Result<()> {
    // The child is the group leader because `process_group(0)` was set in
    // `run_process` before spawning, so `libc::killpg` on the leader's group
    // ID sends SIGKILL to the whole group, not just the child.
    let pgid = child.id() as i32;
    // SAFETY: `killpg` is called with the valid process group ID that this
    // process just created via `CommandExt::process_group(0)` and the
    // `SIGKILL` signal constant. It is an ABI call with no pointer arguments,
    // so there are no aliasing or lifetime concerns.
    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // The process group already exited between `try_wait` and here —
        // that is success, not a harness failure.
        Ok(())
    } else {
        Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("cannot terminate {label} process group {pgid}: {error}"),
        ))
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut std::process::Child, label: &str) -> Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        // The process already exited between `try_wait` and here — that is
        // success, not a harness failure.
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!("cannot terminate {label}: {error}"),
        )),
    }
}

pub(crate) fn drain_stream<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
) -> thread::JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    if buffer.len() < cap {
                        let take = (cap - buffer.len()).min(read);
                        buffer.extend_from_slice(&chunk[..take]);
                        if take < read {
                            truncated = true;
                        }
                    } else {
                        // Keep draining past the cap so the child never blocks
                        // on a full pipe; discard the overflow.
                        truncated = true;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        (buffer, truncated)
    })
}

#[cfg(unix)]
fn termination_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn termination_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

fn locate_node() -> Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolMissing,
            "PATH is unset; cannot locate the Node oracle",
        )
    })?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("node");
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(VerificationError::new(
        ErrorCode::ToolMissing,
        "cannot locate `node` on PATH",
    ))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

pub(crate) fn normalized_env() -> Vec<(String, String)> {
    NORMALIZED_ENV
        .iter()
        .map(|entry| {
            let (key, value) = entry
                .split_once('=')
                .expect("normalized env entry is KEY=VALUE");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).map_err(|error| io_error(path, &error))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        VerificationError::new(
            ErrorCode::Toml,
            format!("{}: TOML must be UTF-8", path.display()),
        )
    })?;
    toml::from_str(text).map_err(|error| {
        VerificationError::new(ErrorCode::Toml, format!("{}: {error}", path.display()))
    })
}

fn require_schema(path: &Path, schema: u32) -> Result<()> {
    if schema == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(schema_error(
            path,
            format!("schema must be {SCHEMA_VERSION}, found {schema}"),
        ))
    }
}

fn require_match(path: &Path, field: &str, actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(schema_error(
            path,
            format!("spec {field} `{actual}` does not match manifest `{expected}`"),
        ))
    }
}

fn require_nonempty(path: &Path, field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(schema_error(path, format!("{field} must be nonempty")))
    } else {
        Ok(())
    }
}

fn require_commit(path: &Path, id: &str, commit: &str) -> Result<()> {
    if is_lower_hex(commit, COMMIT_LEN) {
        Ok(())
    } else {
        Err(schema_error(
            path,
            format!(
                "project `{id}` commit must be a {COMMIT_LEN}-char lowercase hex pin, found `{commit}`"
            ),
        ))
    }
}

fn require_ts_entrypoint(path: &Path, id: &str, entrypoint: &str) -> Result<()> {
    let is_ts = Path::new(entrypoint)
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("ts");
    if is_ts {
        Ok(())
    } else {
        Err(schema_error(
            path,
            format!("project `{id}` entrypoint must be a `.ts` file, found `{entrypoint}`"),
        ))
    }
}

fn require_clean_relative(path: &Path, kind: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(schema_error(path, format!("{kind} path must not be empty")));
    }
    let candidate = Path::new(value);
    let clean = !candidate.is_absolute()
        && candidate
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if clean {
        Ok(())
    } else {
        Err(schema_error(
            path,
            format!("{kind} path `{value}` must be a clean relative path"),
        ))
    }
}

fn require_unique_nonempty(path: &Path, field: &str, values: &[String]) -> Result<()> {
    if values.is_empty() {
        return Err(schema_error(path, format!("{field} must not be empty")));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        if value.trim().is_empty() {
            return Err(schema_error(
                path,
                format!("{field} must not contain empty entries"),
            ));
        }
        if !seen.insert(value.as_str()) {
            return Err(schema_error(
                path,
                format!("{field} has duplicate entry `{value}`"),
            ));
        }
    }
    Ok(())
}

fn require_exact_set(path: &Path, kind: &str, actual: &[String], expected: &[&str]) -> Result<()> {
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
    let mut actual_set = BTreeSet::new();
    for value in actual {
        if !actual_set.insert(value.as_str()) {
            return Err(schema_error(
                path,
                format!("{kind} has duplicate entry `{value}`"),
            ));
        }
    }
    if actual_set == expected_set {
        Ok(())
    } else {
        Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "{}: {kind}: {}",
                path.display(),
                set_difference(&expected_set, &actual_set)
            ),
        ))
    }
}

fn set_difference(expected: &BTreeSet<&str>, actual: &BTreeSet<&str>) -> String {
    let missing: Vec<&str> = expected.difference(actual).copied().collect();
    let extra: Vec<&str> = actual.difference(expected).copied().collect();
    format!(
        "missing [{}]; extra [{}]",
        missing.join(", "),
        extra.join(", ")
    )
}

fn require_dir(root: &Path, spec_path: &Path, relative: &str) -> Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, &error))?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(schema_error(
            spec_path,
            format!("`{relative}` must be a directory"),
        ))
    }
}

fn require_file(root: &Path, spec_path: &Path, relative: &str) -> Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, &error))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(schema_error(
            spec_path,
            format!("`{relative}` must be a regular file"),
        ));
    }
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn spawn_error(label: &str, program: &Path, error: &std::io::Error) -> VerificationError {
    let code = if error.kind() == ErrorKind::NotFound {
        ErrorCode::ToolMissing
    } else {
        ErrorCode::Io
    };
    VerificationError::new(
        code,
        format!("cannot spawn {label} `{}`: {error}", program.display()),
    )
}

fn pipe_error(label: &str, stream: &str) -> VerificationError {
    VerificationError::new(
        ErrorCode::Io,
        format!("{label} {stream} pipe was not captured"),
    )
}

fn wait_error(label: &str, error: std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("cannot wait on {label}: {error}"))
}

fn thread_error(label: &str, stream: &str) -> VerificationError {
    VerificationError::new(
        ErrorCode::ToolFailed,
        format!("{label} {stream} reader thread panicked"),
    )
}

fn io_error(path: &Path, error: &std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

fn schema_error(path: &Path, detail: impl Into<String>) -> VerificationError {
    VerificationError::new(
        ErrorCode::Schema,
        format!("{}: {}", path.display(), detail.into()),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_runtime::Host;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn exit_code_merge_prefers_host_zero() {
        let mut host = bamts_node::NodeHost::new();
        Host::set_exit_code(&mut host, 0);
        assert_eq!(host.completion_exit_code(7), 0);
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root is two levels above the crate")
            .to_path_buf()
    }

    fn scratch(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "bamts-corpus-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn aot_case(id: &str, expected_timeout_ms: u64) -> CaseSpec {
        CaseSpec {
            id: id.to_owned(),
            repository: format!("https://example.com/{id}"),
            commit: "a".repeat(40),
            license: "MIT".into(),
            source_dir: format!("corpus/projects/{id}"),
            entrypoint: format!("corpus/cases/{id}.ts"),
            node_args: Vec::new(),
            expected_timeout_ms,
            constructs: Vec::new(),
            source_files: Vec::new(),
            compiler_args: Vec::new(),
        }
    }

    fn run_script(cwd: &Path, source: &str, limits: OracleLimits) -> OracleOutcome {
        let node = locate_node().expect("node on PATH");
        fs::write(cwd.join("case.js"), source).expect("write script");
        run_node(
            &node,
            cwd,
            &normalized_env(),
            &[OsString::from("case.js")],
            &limits,
        )
        .expect("oracle run")
    }

    // ---- execution modes --------------------------------------------------

    #[test]
    fn execution_mode_all_covers_exactly_three_modes() {
        assert_eq!(
            ExecutionMode::ALL,
            [
                ExecutionMode::Interpreter,
                ExecutionMode::Jit,
                ExecutionMode::Aot
            ]
        );
        let names: BTreeSet<&str> = ExecutionMode::ALL
            .iter()
            .map(|mode| mode.as_str())
            .collect();
        assert_eq!(names, BTreeSet::from(["interpreter", "jit", "aot"]));
    }

    #[test]
    fn corpus_source_kinds_select_javascript_compatibility() {
        let mut spec = aot_case("source-kind", 1);
        spec.source_files = vec!["corpus/projects/source-kind/index.ts".into()];
        assert!(!case_requires_javascript_compatibility(&spec));

        spec.source_files = vec!["corpus/projects/source-kind/index.js".into()];
        assert!(case_requires_javascript_compatibility(&spec));

        spec.source_files = vec!["corpus/projects/source-kind/index.mjs".into()];
        assert!(case_requires_javascript_compatibility(&spec));

        spec.source_files.clear();
        spec.entrypoint = "corpus/cases/source-kind.jsx".into();
        assert!(case_requires_javascript_compatibility(&spec));

        spec.entrypoint = "corpus/cases/source-kind.tsx".into();
        assert!(!case_requires_javascript_compatibility(&spec));
    }

    #[test]
    fn worker_cli_args_preserve_javascript_compatibility_per_backend() {
        let jit = cli_args(
            Mode::Run,
            ExecutionTarget::Jit,
            PathBuf::from("corpus/cases/javascript.js"),
            None,
            true,
            &[],
        )
        .expect("JIT CLI args parse");
        assert!(jit.js_compat.enabled);

        let output = Path::new("target/corpus-differential/javascript");
        let aot = cli_args(
            Mode::Compile,
            ExecutionTarget::Aot,
            PathBuf::from("corpus/cases/javascript.jsx"),
            Some(output),
            true,
            &[],
        )
        .expect("AOT CLI args parse");
        assert!(aot.js_compat.enabled);
        assert_eq!(aot.output.file.as_deref(), output.to_str());
        let typescript = cli_args(
            Mode::Run,
            ExecutionTarget::Jit,
            PathBuf::from("corpus/cases/typescript.ts"),
            None,
            false,
            &[],
        )
        .expect("TypeScript CLI args parse");
        assert!(!typescript.js_compat.enabled);
    }

    // ---- manifest / spec parsing -----------------------------------------

    #[test]
    fn load_corpus_accepts_the_real_repository() {
        let corpus = load_corpus(&repo_root()).expect("real corpus validates");
        assert_eq!(corpus.cases.len(), corpus.manifest.projects.len());
        let manifest_ids: Vec<&str> = corpus
            .manifest
            .projects
            .iter()
            .map(|project| project.id.as_str())
            .collect();
        assert_eq!(manifest_ids, PINNED_CASE_IDS);
        let pinned: BTreeSet<&str> = PINNED_CASE_IDS.into_iter().collect();
        for (name, subset) in [
            ("synchronous", TASK_106_SYNC_CASE_IDS.as_slice()),
            ("node", TASK_107_NODE_CASE_IDS.as_slice()),
        ] {
            assert!(
                subset.iter().all(|id| pinned.contains(id)),
                "{name} case IDs must be a subset of PINNED_CASE_IDS"
            );
        }
        assert!(corpus.case("tiny-invariant").is_some());
    }

    #[test]
    fn exact_layout_rejects_orphan_and_missing_files() {
        let manifest = CorpusManifest {
            node_version: NODE_VERSION.to_owned(),
            environment: NORMALIZED_ENV.iter().map(|s| s.to_string()).collect(),
            compare: COMPARE_KEYS.iter().map(|s| s.to_string()).collect(),
            projects: vec![ManifestProject {
                id: "only".into(),
                repository: "https://example.com/only".into(),
                commit: "a".repeat(40),
                spec: "corpus/specs/only.toml".into(),
                entrypoint: "corpus/cases/only.ts".into(),
            }],
        };
        let root = scratch("layout");
        fs::create_dir_all(root.join("corpus/specs")).unwrap();
        fs::create_dir_all(root.join("corpus/cases")).unwrap();
        fs::write(root.join("corpus/specs/only.toml"), "x").unwrap();
        fs::write(root.join("corpus/cases/only.ts"), "x").unwrap();
        // Exact match passes.
        assert!(verify_exact_layout(&root, &manifest).is_ok());
        // An orphan spec not named by the manifest is drift.
        fs::write(root.join("corpus/specs/orphan.toml"), "x").unwrap();
        assert_eq!(
            verify_exact_layout(&root, &manifest).unwrap_err().code(),
            ErrorCode::SetMismatch
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_rejects_unknown_field() {
        let manifest = format!(
            "schema = 1\nnode_version = \"{NODE_VERSION}\"\nenvironment = {env:?}\ncompare = {cmp:?}\nrogue = true\n[[projects]]\nid=\"a\"\nrepository=\"r\"\ncommit=\"{c}\"\nspec=\"corpus/specs/a.toml\"\nentrypoint=\"corpus/cases/a.ts\"\n",
            env = NORMALIZED_ENV,
            cmp = COMPARE_KEYS,
            c = "a".repeat(40),
        );
        let err = toml::from_str::<RawManifest>(&manifest)
            .err()
            .expect("unknown field rejected");
        assert!(err.to_string().contains("rogue"), "{err}");
    }

    #[test]
    fn manifest_rejects_wrong_schema_and_node_version() {
        let base = |schema: u32, version: &str| RawManifest {
            schema,
            node_version: version.to_owned(),
            environment: NORMALIZED_ENV.iter().map(|s| s.to_string()).collect(),
            compare: COMPARE_KEYS.iter().map(|s| s.to_string()).collect(),
            projects: vec![RawProject {
                id: "a".into(),
                repository: "r".into(),
                commit: "a".repeat(40),
                spec: "corpus/specs/a.toml".into(),
                entrypoint: "corpus/cases/a.ts".into(),
            }],
        };
        let path = Path::new("manifest.toml");
        assert_eq!(
            validate_manifest(path, base(2, NODE_VERSION))
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
        assert_eq!(
            validate_manifest(path, base(1, "20.0.0"))
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn manifest_rejects_environment_and_compare_mismatch() {
        let make = |environment: Vec<String>, compare: Vec<String>| RawManifest {
            schema: 1,
            node_version: NODE_VERSION.to_owned(),
            environment,
            compare,
            projects: vec![RawProject {
                id: "a".into(),
                repository: "r".into(),
                commit: "a".repeat(40),
                spec: "corpus/specs/a.toml".into(),
                entrypoint: "corpus/cases/a.ts".into(),
            }],
        };
        let path = Path::new("manifest.toml");
        let canon_env: Vec<String> = NORMALIZED_ENV.iter().map(|s| s.to_string()).collect();
        let canon_cmp: Vec<String> = COMPARE_KEYS.iter().map(|s| s.to_string()).collect();

        // Missing one env entry.
        let short_env = canon_env[..3].to_vec();
        assert_eq!(
            validate_manifest(path, make(short_env, canon_cmp.clone()))
                .unwrap_err()
                .code(),
            ErrorCode::SetMismatch
        );
        // Extra env entry.
        let mut long_env = canon_env.clone();
        long_env.push("EXTRA=1".into());
        assert_eq!(
            validate_manifest(path, make(long_env, canon_cmp.clone()))
                .unwrap_err()
                .code(),
            ErrorCode::SetMismatch
        );
        // Duplicate env entry.
        let mut dup_env = canon_env.clone();
        dup_env[3] = dup_env[0].clone();
        assert_eq!(
            validate_manifest(path, make(dup_env, canon_cmp.clone()))
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
        // Compare key mismatch.
        assert_eq!(
            validate_manifest(
                path,
                make(canon_env, vec!["stdout".into(), "stderr".into()])
            )
            .unwrap_err()
            .code(),
            ErrorCode::SetMismatch
        );
    }

    #[test]
    fn manifest_rejects_bad_pins_and_paths_and_duplicates() {
        let project = |id: &str, commit: &str, spec: &str, entry: &str| RawProject {
            id: id.into(),
            repository: "r".into(),
            commit: commit.into(),
            spec: spec.into(),
            entrypoint: entry.into(),
        };
        let make = |projects: Vec<RawProject>| RawManifest {
            schema: 1,
            node_version: NODE_VERSION.to_owned(),
            environment: NORMALIZED_ENV.iter().map(|s| s.to_string()).collect(),
            compare: COMPARE_KEYS.iter().map(|s| s.to_string()).collect(),
            projects,
        };
        let path = Path::new("manifest.toml");
        let good = "a".repeat(40);

        // Short commit.
        assert_eq!(
            validate_manifest(
                path,
                make(vec![project(
                    "a",
                    &"a".repeat(39),
                    "corpus/specs/a.toml",
                    "corpus/cases/a.ts"
                )])
            )
            .unwrap_err()
            .code(),
            ErrorCode::Schema
        );
        // Uppercase commit hex.
        assert_eq!(
            validate_manifest(
                path,
                make(vec![project(
                    "a",
                    &"A".repeat(40),
                    "corpus/specs/a.toml",
                    "corpus/cases/a.ts"
                )])
            )
            .unwrap_err()
            .code(),
            ErrorCode::Schema
        );
        // Path traversal in spec.
        assert_eq!(
            validate_manifest(
                path,
                make(vec![project(
                    "a",
                    &good,
                    "../evil.toml",
                    "corpus/cases/a.ts"
                )])
            )
            .unwrap_err()
            .code(),
            ErrorCode::Schema
        );
        // Non-.ts entrypoint.
        assert_eq!(
            validate_manifest(
                path,
                make(vec![project(
                    "a",
                    &good,
                    "corpus/specs/a.toml",
                    "corpus/cases/a.js"
                )])
            )
            .unwrap_err()
            .code(),
            ErrorCode::Schema
        );
        // Duplicate project id.
        assert_eq!(
            validate_manifest(
                path,
                make(vec![
                    project("a", &good, "corpus/specs/a.toml", "corpus/cases/a.ts"),
                    project("a", &good, "corpus/specs/b.toml", "corpus/cases/b.ts"),
                ])
            )
            .unwrap_err()
            .code(),
            ErrorCode::Schema
        );
    }

    fn valid_project() -> ManifestProject {
        ManifestProject {
            id: "sample".into(),
            repository: "https://example.com/sample".into(),
            commit: "a".repeat(40),
            spec: "corpus/specs/sample.toml".into(),
            entrypoint: "corpus/cases/sample.ts".into(),
        }
    }

    fn valid_raw_spec() -> RawSpec {
        RawSpec {
            schema: 1,
            id: "sample".into(),
            repository: "https://example.com/sample".into(),
            commit: "a".repeat(40),
            license: "MIT".into(),
            source_dir: "corpus/projects/sample".into(),
            entrypoint: "corpus/cases/sample.ts".into(),
            node_args: vec![],
            expected_timeout_ms: 5000,
            constructs: vec!["one".into(), "two".into()],
            source_files: vec!["corpus/projects/sample/index.ts".into()],
            compiler_args: vec![],
        }
    }

    #[test]
    fn spec_cross_checks_against_project() {
        let path = Path::new("spec.toml");
        let project = valid_project();
        // Happy path.
        assert!(validate_spec(path, valid_raw_spec(), &project).is_ok());

        // id mismatch.
        let mut raw = valid_raw_spec();
        raw.id = "other".into();
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );

        // commit mismatch.
        let mut raw = valid_raw_spec();
        raw.commit = "b".repeat(40);
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );

        // entrypoint mismatch.
        let mut raw = valid_raw_spec();
        raw.entrypoint = "corpus/cases/other.ts".into();
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn spec_rejects_boundary_values() {
        let path = Path::new("spec.toml");
        let project = valid_project();

        // Zero timeout.
        let mut raw = valid_raw_spec();
        raw.expected_timeout_ms = 0;
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // Over-cap timeout.
        let mut raw = valid_raw_spec();
        raw.expected_timeout_ms = MAX_TIMEOUT_MS + 1;
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // Empty constructs.
        let mut raw = valid_raw_spec();
        raw.constructs = vec![];
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // Duplicate construct.
        let mut raw = valid_raw_spec();
        raw.constructs = vec!["dup".into(), "dup".into()];
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // Empty source_files.
        let mut raw = valid_raw_spec();
        raw.source_files = vec![];
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // source_file traversal.
        let mut raw = valid_raw_spec();
        raw.source_files = vec!["../evil".into()];
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
        // Empty node_args entry.
        let mut raw = valid_raw_spec();
        raw.node_args = vec![String::new()];
        assert_eq!(
            validate_spec(path, raw, &project).unwrap_err().code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn spec_rejects_harness_owned_compiler_args() {
        let path = Path::new("spec.toml");
        let project = valid_project();

        for argument in HARNESS_OWNED_ARGS
            .iter()
            .flat_map(|argument| [(*argument).to_owned(), format!("{argument}=value")])
            .chain([String::new(), " \t".to_owned()])
        {
            let mut raw = valid_raw_spec();
            raw.compiler_args = vec![argument.clone()];
            let error = validate_spec(path, raw, &project).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Schema);
            assert!(
                error.to_string().contains(&format!("`{argument}`"))
                    || argument.trim().is_empty() && error.to_string().contains("empty"),
                "{error}"
            );
        }

        let mut raw = valid_raw_spec();
        raw.compiler_args = vec!["-A".into(), "no-with".into()];
        let spec = validate_spec(path, raw, &project).expect("unowned compiler args");
        assert_eq!(spec.compiler_args, ["-A", "no-with"]);
    }

    // ---- oracle behavior --------------------------------------------------

    #[test]
    fn verify_node_version_accepts_pinned_and_rejects_missing() {
        let node = locate_node().expect("node on PATH");
        verify_node_version(&node).expect("pinned node verifies");

        let err = verify_node_version(Path::new("/nonexistent/definitely-not-node")).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ToolMissing);
    }

    #[test]
    fn oracle_captures_raw_stdout_and_exact_exit_code() {
        let dir = scratch("stdout");
        let outcome = run_script(
            &dir,
            "process.stdout.write('hello');process.exit(3);",
            OracleLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            },
        );
        assert!(!outcome.timed_out);
        assert_eq!(outcome.stdout, b"hello");
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.parity_key(), (b"hello".as_slice(), Some(3)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oracle_normalizes_environment() {
        let dir = scratch("env");
        let outcome = run_script(
            &dir,
            "process.stdout.write([process.env.TZ,process.env.NO_COLOR,process.env.PATH].map(String).join(','));",
            OracleLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            },
        );
        assert_eq!(outcome.stdout, b"UTC,1,undefined");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oracle_bounds_output_without_deadlock() {
        let dir = scratch("bound");
        let outcome = run_script(
            &dir,
            "process.stdout.write('x'.repeat(100000));",
            OracleLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: 1000,
            },
        );
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.len(), 1000);
        assert!(outcome.stdout_truncated);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oracle_enforces_timeout() {
        let dir = scratch("timeout");
        let outcome = run_script(
            &dir,
            "while(true){}",
            OracleLimits {
                timeout: Duration::from_millis(200),
                max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            },
        );
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn terminate_process_group_kills_grandchild() {
        let dir = scratch("killpg-grandchild");
        let script = "echo $$ > child.pid\n\
                      sh -c 'while :; do sleep 0.1; done' &\n\
                      echo $! > grandchild.pid\n\
                      while :; do sleep 0.1; done\n";
        fs::write(dir.join("child.sh"), script).expect("write child script");

        let outcome = run_process(
            "killpg test",
            Path::new("/bin/sh"),
            &dir,
            &[("PATH".to_string(), "/usr/bin:/bin".to_string())],
            &[OsString::from("child.sh")],
            &OracleLimits {
                timeout: Duration::from_millis(1000),
                max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            },
        )
        .expect("run process");

        assert!(outcome.timed_out, "process group should time out");
        assert_eq!(outcome.signal, Some(9), "process group should be SIGKILLed");

        let child_pid = fs::read_to_string(dir.join("child.pid"))
            .expect("child pid file")
            .trim()
            .parse::<i32>()
            .expect("child pid is numeric");
        let grandchild_pid = fs::read_to_string(dir.join("grandchild.pid"))
            .expect("grandchild pid file")
            .trim()
            .parse::<i32>()
            .expect("grandchild pid is numeric");

        fn process_exists(pid: i32) -> bool {
            std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .expect("kill -0")
                .success()
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut child_gone = false;
        let mut grandchild_gone = false;
        while Instant::now() < deadline {
            if !child_gone && !process_exists(child_pid) {
                child_gone = true;
            }
            if !grandchild_gone && !process_exists(grandchild_pid) {
                grandchild_gone = true;
            }
            if child_gone && grandchild_gone {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        assert!(child_gone, "child {child_pid} should be dead");
        assert!(
            grandchild_gone,
            "grandchild {grandchild_pid} should be dead"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interpreter_fuel_tracks_the_selected_wall_time_budget() {
        let short = interpreter_limits(Duration::from_millis(25));
        let long = interpreter_limits(Duration::from_millis(250));

        assert_eq!(short.fuel, 25 * INTERPRETER_FUEL_PER_MILLISECOND);
        assert_eq!(long.fuel, 250 * INTERPRETER_FUEL_PER_MILLISECOND);
        assert_eq!(long.fuel, short.fuel * 10);
    }

    #[test]
    fn worker_env_excludes_path_for_in_process_modes() {
        // JIT and Interpreter execute the case program in-process, so they
        // must not inherit PATH — only the AOT compile step needs a
        // discoverable toolchain for the linker.
        let has_path = |env: &[(String, String)]| env.iter().any(|(key, _)| key == "PATH");
        assert!(
            !has_path(&worker_env(WorkerOperation::Jit)),
            "JIT worker must not expose PATH to the case program"
        );
        assert!(
            !has_path(&worker_env(WorkerOperation::Interpreter)),
            "interpreter worker must not expose PATH to the case program"
        );
        assert!(
            has_path(&worker_env(WorkerOperation::AotCompile)),
            "AOT compile worker needs PATH for the linker"
        );
    }

    #[test]
    fn aot_executable_uses_only_the_case_budget_remaining_after_compile() {
        let total = Duration::from_millis(250);

        assert_eq!(
            remaining_case_budget(total, Duration::from_millis(123)),
            Some(Duration::from_millis(127))
        );
        assert_eq!(remaining_case_budget(total, total), None);
        assert_eq!(
            remaining_case_budget(total, Duration::from_millis(251)),
            None
        );
    }

    #[test]
    fn aot_executable_preserves_output_limit_with_remaining_budget() {
        let spec = aot_case("aot-budget", 250);
        let remaining = remaining_case_budget(spec.timeout(), Duration::from_millis(123))
            .expect("compile has remaining budget");

        let limits = aot_execution_limits(remaining, 123);
        assert_eq!(limits.timeout, Duration::from_millis(127));
        assert_eq!(limits.max_output_bytes, 123);
    }

    #[test]
    fn live_aot_artifact_directories_never_overlap() {
        let root = scratch("aot-artifacts");
        let spec = aot_case("same-case", 250);
        let first = ArtifactDirectory::create(&root, &spec, ExecutionMode::Aot)
            .expect("create first directory");
        let second = ArtifactDirectory::create(&root, &spec, ExecutionMode::Aot)
            .expect("create second directory");
        let first_executable = first.executable(&spec);
        let second_executable = second.executable(&spec);
        assert_ne!(first_executable, second_executable);
        fs::write(&first_executable, b"first").expect("write first artifact");
        fs::write(&second_executable, b"second").expect("write second artifact");
        drop(first);
        assert_eq!(
            fs::read(&second_executable).expect("second artifact remains live"),
            b"second"
        );
        drop(second);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn aot_compile_evidence_cannot_fake_a_successful_timeout_exit() {
        let runtime = OracleOutcome {
            timed_out: true,
            exit_code: None,
            signal: Some(9),
            stdout: b"runtime stdout".to_vec(),
            stdout_truncated: false,
            stderr: b"runtime stderr".to_vec(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        };

        let outcome = with_aot_compile_evidence(runtime, b"compile warning".to_vec(), false, 128);
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.stdout, b"runtime stdout");
        assert_eq!(outcome.stderr, b"runtime stderr");
        assert_eq!(outcome.compile_stderr, b"compile warning");
    }

    #[test]
    fn oracle_treats_stderr_as_evidence_not_parity() {
        let dir = scratch("stderr");
        let outcome = run_script(
            &dir,
            "process.stderr.write('warning');process.stdout.write('ok');",
            OracleLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            },
        );
        assert_eq!(outcome.stdout, b"ok");
        assert_eq!(outcome.stderr, b"warning");
        assert_eq!(outcome.parity_key().0, b"ok");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parity_never_affirms_unreliable_outcomes() {
        let reliable = |stdout: &[u8], code: i32| OracleOutcome {
            timed_out: false,
            exit_code: Some(code),
            signal: None,
            stdout: stdout.to_vec(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        };

        // Two clean, identical runs agree.
        assert!(reliable(b"same", 0).parity_matches(&reliable(b"same", 0)));
        // Divergent stdout or exit code does not.
        assert!(!reliable(b"a", 0).parity_matches(&reliable(b"b", 0)));
        assert!(!reliable(b"same", 0).parity_matches(&reliable(b"same", 1)));

        // Two timed-out runs (both exit_code None) must NOT be reported as a
        // match despite equal keys — a kill is a non-answer.
        let killed = OracleOutcome {
            timed_out: true,
            exit_code: None,
            signal: Some(9),
            stdout: Vec::new(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        };
        assert!(!killed.is_reliable());
        assert!(!killed.parity_matches(&killed));

        // Truncated stdout only exposes a prefix; equal prefixes never match.
        let truncated = OracleOutcome {
            timed_out: false,
            exit_code: Some(0),
            signal: None,
            stdout: b"prefix".to_vec(),
            stdout_truncated: true,
            stderr: Vec::new(),
            stderr_truncated: false,
            compile_stderr: Vec::new(),
            compile_stderr_truncated: false,
        };
        assert!(!truncated.is_reliable());
        assert!(!truncated.parity_matches(&truncated));
        assert!(!truncated.parity_matches(&reliable(b"prefix", 0)));
    }

    #[test]
    fn oracle_runs_a_real_corpus_case() {
        let root = repo_root();
        let corpus = load_corpus(&root).expect("corpus validates");
        let oracle = NodeOracle::discover(&root).expect("discover pinned node");
        let case = corpus.case("tiny-invariant").expect("case present");
        let outcome = oracle.run_case(case).expect("case runs");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            outcome.stdout.windows(6).any(|window| window == b"truthy"),
            "stdout should contain project-derived output"
        );
    }

    #[test]
    fn worker_exit_zero_without_response_fails_with_tool_failed() {
        let root = repo_root();
        let spec = aot_case("missing-response", 10_000);
        let artifacts = ArtifactDirectory::create(&root, &spec, ExecutionMode::Jit)
            .expect("create scratch artifacts");
        let request = WorkerRequest {
            root,
            spec,
            operation: WorkerOperation::Jit,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            executable: None,
        };
        let error = match run_worker(&artifacts, &request, Duration::from_secs(5)) {
            Err(error) => error,
            Ok(_) => panic!("worker without response must fail"),
        };
        assert_eq!(error.code(), ErrorCode::ToolFailed);
        let message = error.to_string();
        assert!(
            message.contains("wrote no response"),
            "error should report missing response: {message}"
        );
        assert!(
            message.contains(CORPUS_WORKER_TEST),
            "error should name the worker test: {message}"
        );
        assert!(
            message.contains("run_corpus_worker_from_env"),
            "error should name the worker entry point: {message}"
        );
        assert!(
            !message.contains("No such file or directory"),
            "error must not leak a filesystem I/O message: {message}"
        );
    }

    #[test]
    fn aot_compile_evidence_preserves_worker_truncation() {
        let runtime = timeout_outcome(Vec::new(), 4);
        let outcome = with_aot_compile_evidence(runtime, b"warn".to_vec(), true, 4);
        assert_eq!(outcome.compile_stderr, b"warn");
        assert!(outcome.compile_stderr_truncated);
    }

    #[test]
    fn manifest_pinned_ids_and_subsets() {
        let path = Path::new("manifest.toml");
        let project = |id: &str| ManifestProject {
            id: id.into(),
            repository: "https://example.com".into(),
            commit: "a".repeat(40),
            spec: "corpus/specs/x.toml".into(),
            entrypoint: "corpus/cases/x.ts".into(),
        };
        let projects: Vec<_> = PINNED_CASE_IDS.iter().copied().map(project).collect();
        assert!(verify_pinned_case_ids(path, &projects).is_ok());

        let mut wrong_order = projects.clone();
        if wrong_order.len() >= 2 {
            wrong_order.swap(0, 1);
        }
        assert_eq!(
            verify_pinned_case_ids(path, &wrong_order)
                .unwrap_err()
                .code(),
            ErrorCode::SetMismatch
        );

        let mut extra = projects;
        extra.push(project("extra"));
        assert_eq!(
            verify_pinned_case_ids(path, &extra).unwrap_err().code(),
            ErrorCode::SetMismatch
        );
    }

    #[test]
    fn cli_arg_strings_are_harness_owned() {
        let entrypoint = PathBuf::from("corpus/cases/sample.ts");
        let output = Path::new("target/corpus-differential/sample");
        for mode in [Mode::Check, Mode::Compile, Mode::Run, Mode::Explain] {
            for target in [ExecutionTarget::Aot, ExecutionTarget::Jit] {
                for js in [false, true] {
                    for out in [None, Some(output)] {
                        let strings =
                            cli_arg_strings(mode, target, entrypoint.clone(), out, js, &[]);
                        let entrypoint_str = entrypoint.to_string_lossy().into_owned();
                        let output_str = out.map(|p| p.to_string_lossy().into_owned());
                        for token in &strings {
                            if token == &entrypoint_str || output_str.as_ref() == Some(token) {
                                continue;
                            }
                            assert!(
                                HARNESS_OWNED_ARGS.contains(&token.as_str()),
                                "`{token}` emitted by cli_arg_strings is not in HARNESS_OWNED_ARGS"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compiler_args_reject_emitted_flags_and_whitespace() {
        let path = Path::new("spec.toml");
        for token in [
            "run",
            "compile",
            "aot",
            "jit",
            "--target",
            "--output",
            "--js-compat",
        ] {
            assert_eq!(
                validate_compiler_args(path, &[token.to_owned()])
                    .unwrap_err()
                    .code(),
                ErrorCode::Schema
            );
            assert_eq!(
                validate_compiler_args(path, &[format!("{token}=value")])
                    .unwrap_err()
                    .code(),
                ErrorCode::Schema
            );
        }
        assert_eq!(
            validate_compiler_args(path, &[" \t".to_owned()])
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
        assert_eq!(
            validate_compiler_args(path, &["  --target  ".to_owned()])
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
        assert!(validate_compiler_args(path, &["--strict".to_owned()]).is_ok());
    }

    #[test]
    fn bounded_output_caps_and_flags_truncation() {
        let (out, truncated) = bounded_output(vec![0, 1, 2, 3, 4], 3);
        assert_eq!(out, vec![0, 1, 2]);
        assert!(truncated);

        let (out, truncated) = bounded_output(vec![0, 1, 2], 5);
        assert_eq!(out, vec![0, 1, 2]);
        assert!(!truncated);
    }
}
