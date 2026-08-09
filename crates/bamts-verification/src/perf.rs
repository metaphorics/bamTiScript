//! Performance harness schemas and the `perf_budget` measurement/comparison
//! logic (U0.8 / PB-1).
//!
//! This module owns three schema records — the pinned host fingerprint
//! ([`HostManifest`]), the benchmark manifest ([`BenchmarkManifest`]), and the
//! budget policy ([`BudgetPolicy`]) — plus the [`MeasureResult`] the runner
//! writes and `compare` re-reads.
//!
//! Host identity is matched strictly: the immutable hardware/OS fields in
//! [`HostFingerprint`] must equal the live machine or the run is
//! [`PerfErrorCode::InvalidHost`]. CPU governor and swap are runtime-tunable
//! measurement preconditions (BH1 "Execution Environment Rules"): they are
//! read, recorded, and surfaced via [`MeasureResult::conditions_match`], but
//! they do not gate host identity, so a BH1-hardware host that has not yet been
//! tuned still measures (the operator gates on `conditions_match`).
//!
//! For S0 the measurement drives [`crate::suite::run_suite_with_telemetry`] for
//! the requested slice. `total` is the measured seam wall; the `parse`, `bind`,
//! `check`, and `emit` keys (plus the optional `scan`) carry the real frontend
//! phase telemetry aggregated across every check-backend compile the runner
//! performs. A successful run whose result omits any required phase key is a
//! [`PerfErrorCode::HarnessError`] (see [`measure`]).

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    time::Instant,
};

use bamts_compiler::telemetry::Phase;
use serde::{Deserialize, Serialize};

use crate::suite::{BackendFilter, RunFilterOptions, StatusFilter, run_suite_with_telemetry};

/// Schema version accepted by every perf schema loader.
pub const PERF_SCHEMA_VERSION: u32 = 1;
/// Default repeats when a benchmark omits the field.
pub const DEFAULT_REPEATS: u32 = 3;
/// Phase keys every measured result must carry (U0.9 fills real telemetry).
pub const PHASE_KEYS: [&str; 5] = ["parse", "bind", "check", "emit", "total"];

/// Terminal `perf_budget` failure code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfErrorCode {
    /// The current machine does not match the pinned host identity.
    InvalidHost,
    /// A baseline was requested but is missing.
    NoBaseline,
    /// A budget threshold was exceeded.
    BudgetBreach,
    /// The harness could not complete measurement.
    HarnessError,
    /// Command-line usage error.
    Usage,
}

impl PerfErrorCode {
    /// Stable token printed to stderr and matched by callers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidHost => "INVALID_HOST",
            Self::NoBaseline => "NO_BASELINE",
            Self::BudgetBreach => "BUDGET_BREACH",
            Self::HarnessError => "HARNESS_ERROR",
            Self::Usage => "USAGE",
        }
    }

    /// Process exit code for this failure.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::InvalidHost => 3,
            Self::NoBaseline => 4,
            Self::BudgetBreach => 5,
            Self::HarnessError => 6,
        }
    }
}

/// A `perf_budget` error carrying a terminal [`PerfErrorCode`] and detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfError {
    /// The terminal failure class.
    pub code: PerfErrorCode,
    /// Human-readable detail.
    pub detail: String,
}

impl PerfError {
    /// Builds a new error.
    #[must_use]
    pub fn new(code: PerfErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Convenience constructor for [`PerfErrorCode::HarnessError`].
    #[must_use]
    pub fn harness(detail: impl Into<String>) -> Self {
        Self::new(PerfErrorCode::HarnessError, detail)
    }

    /// Convenience constructor for [`PerfErrorCode::Usage`].
    #[must_use]
    pub fn usage(detail: impl Into<String>) -> Self {
        Self::new(PerfErrorCode::Usage, detail)
    }
}

impl fmt::Display for PerfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for PerfError {}

/// Module-local result type.
pub type Result<T, E = PerfError> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Host fingerprint schema
// ---------------------------------------------------------------------------

/// A pinned benchmark host manifest (`perf/hosts/bh1.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostManifest {
    /// Schema version.
    pub schema: u32,
    /// Host identifier (for example `BH1`).
    pub host: String,
    /// Immutable hardware/OS identity, matched strictly.
    pub fingerprint: HostFingerprint,
    /// Runtime measurement conditions, matched strictly.
    pub conditions: HostConditions,
    /// Evidence source for each field.
    pub source: BTreeMap<String, String>,
}

impl HostManifest {
    /// Confirms the immutable hardware/OS identity matches the machine.
    ///
    /// Runtime conditions (governor/swap) are advisory and reported separately
    /// via [`MeasureResult::conditions_match`]; they never yield `INVALID_HOST`.
    ///
    /// # Errors
    /// Returns [`PerfErrorCode::InvalidHost`] naming every mismatched identity field.
    pub fn require_match(&self, machine: &MachineFingerprint) -> Result<()> {
        require_no_host_mismatches(self.fingerprint.identity_mismatches(machine))
    }
}

/// The immutable hardware/OS identity of a benchmark host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostFingerprint {
    /// `uname -r` kernel release, for example `7.0.0-28-generic`.
    pub kernel_release: String,
    /// Kernel build version tag (advisory; compared leniently).
    pub kernel_version: String,
    /// CPU architecture, for example `x86_64`.
    pub arch: String,
    /// CPU model name from `/proc/cpuinfo`.
    pub cpu_model: String,
    /// Distinct physical CPU sockets.
    pub sockets: u32,
    /// Physical cores per socket.
    pub cores_per_socket: u32,
    /// CPU microcode revision.
    pub microcode: String,
}

impl HostFingerprint {
    /// Verifies the immutable identity fields against a live machine reading.
    ///
    /// Returns the list of mismatched field names; empty means a match.
    /// `kernel_version` is compared leniently (substring) because the live
    /// string carries a build timestamp.
    #[must_use]
    pub fn identity_mismatches(&self, machine: &MachineFingerprint) -> Vec<String> {
        let mut diffs = Vec::new();
        if self.kernel_release != machine.kernel_release {
            diffs.push("kernel_release".to_owned());
        }
        if self.arch != machine.arch {
            diffs.push("arch".to_owned());
        }
        if self.cpu_model != machine.cpu_model {
            diffs.push("cpu_model".to_owned());
        }
        if self.sockets != machine.sockets {
            diffs.push("sockets".to_owned());
        }
        if self.cores_per_socket != machine.cores_per_socket {
            diffs.push("cores_per_socket".to_owned());
        }
        if self.microcode != machine.microcode {
            diffs.push("microcode".to_owned());
        }
        if !machine.kernel_version.contains(&self.kernel_version) {
            diffs.push("kernel_version".to_owned());
        }
        diffs
    }

    /// Confirms the machine matches this identity, or returns `INVALID_HOST`.
    ///
    /// # Errors
    /// Returns [`PerfErrorCode::InvalidHost`] naming the mismatched fields.
    pub fn require_match(&self, machine: &MachineFingerprint) -> Result<()> {
        require_no_host_mismatches(self.identity_mismatches(machine))
    }
}

fn require_no_host_mismatches(diffs: Vec<String>) -> Result<()> {
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(PerfError::new(
            PerfErrorCode::InvalidHost,
            format!("host fingerprint mismatch on: {}", diffs.join(", ")),
        ))
    }
}

/// Runtime measurement conditions BH1 must be tuned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConditions {
    /// Required CPU frequency governor, for example `performance`.
    pub governor: String,
    /// Required total swap in KiB (BH1 mandates `0`).
    pub swap_total_kib: u64,
    /// NUMA node 0 CPU pinning range.
    pub numa_node0_cpus: String,
}

/// Conditions observed on the live machine at measurement time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedConditions {
    /// Live CPU governor.
    pub governor: String,
    /// Live total swap in KiB.
    pub swap_total_kib: u64,
}

impl ObservedConditions {
    /// Whether observed conditions satisfy the manifest preconditions.
    #[must_use]
    pub fn satisfies(&self, expected: &HostConditions) -> bool {
        self.governor == expected.governor && self.swap_total_kib == expected.swap_total_kib
    }
}

/// A live machine fingerprint read from `/proc` and `/sys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFingerprint {
    /// `uname -r` kernel release.
    pub kernel_release: String,
    /// Kernel build version tail (from `#` onward).
    pub kernel_version: String,
    /// CPU architecture.
    pub arch: String,
    /// CPU model name.
    pub cpu_model: String,
    /// Distinct physical sockets.
    pub sockets: u32,
    /// Physical cores per socket.
    pub cores_per_socket: u32,
    /// CPU microcode revision.
    pub microcode: String,
    /// Live governor.
    pub governor: String,
    /// Live total swap (KiB).
    pub swap_total_kib: u64,
}

/// Parses the kernel release and build-version tail from `/proc/version`.
fn parse_proc_version(content: &str) -> Result<(String, String)> {
    let mut tokens = content.split_whitespace();
    // Expected: "Linux version <release> ..."
    let release = tokens
        .nth(2)
        .ok_or_else(|| PerfError::harness("/proc/version missing kernel release"))?
        .to_owned();
    let version = content
        .find('#')
        .map(|start| content[start..].trim_end().to_owned())
        .unwrap_or_default();
    Ok((release, version))
}

/// Parses `(cpu_model, sockets, cores_per_socket, microcode)` from cpuinfo.
fn parse_proc_cpuinfo(content: &str) -> Result<(String, u32, u32, String)> {
    let mut model = None;
    let mut microcode = None;
    let mut cores_per_socket = None;
    let mut sockets = BTreeSet::new();

    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "model name" if model.is_none() => model = Some(value.to_owned()),
            "microcode" if microcode.is_none() => microcode = Some(value.to_owned()),
            "cpu cores" if cores_per_socket.is_none() => cores_per_socket = value.parse().ok(),
            "physical id" => {
                sockets.insert(value.to_owned());
            }
            _ => {}
        }
    }

    let model = model.ok_or_else(|| PerfError::harness("/proc/cpuinfo missing model name"))?;
    let microcode =
        microcode.ok_or_else(|| PerfError::harness("/proc/cpuinfo missing microcode"))?;
    let cores_per_socket =
        cores_per_socket.ok_or_else(|| PerfError::harness("/proc/cpuinfo missing cpu cores"))?;
    let socket_count = u32::try_from(sockets.len())
        .map_err(|_| PerfError::harness("/proc/cpuinfo socket count overflow"))?
        .max(1);
    Ok((model, socket_count, cores_per_socket, microcode))
}

/// Parses `SwapTotal` (KiB) from `/proc/meminfo`.
fn parse_proc_meminfo_swap(content: &str) -> Result<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("SwapTotal:") {
            let value = rest
                .split_whitespace()
                .next()
                .ok_or_else(|| PerfError::harness("/proc/meminfo SwapTotal malformed"))?;
            return value
                .parse()
                .map_err(|_| PerfError::harness("/proc/meminfo SwapTotal not a number"));
        }
    }
    Err(PerfError::harness("/proc/meminfo missing SwapTotal"))
}

/// Reads the live machine fingerprint from `/proc` and `/sys`.
///
/// # Errors
/// Returns [`PerfErrorCode::HarnessError`] if any evidence file cannot be read
/// or parsed.
pub fn read_machine_fingerprint() -> Result<MachineFingerprint> {
    let version = read_evidence("/proc/version")?;
    let cpuinfo = read_evidence("/proc/cpuinfo")?;
    let meminfo = read_evidence("/proc/meminfo")?;
    let governor = read_evidence("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());

    let (kernel_release, kernel_version) = parse_proc_version(&version)?;
    let (cpu_model, sockets, cores_per_socket, microcode) = parse_proc_cpuinfo(&cpuinfo)?;
    let swap_total_kib = parse_proc_meminfo_swap(&meminfo)?;

    Ok(MachineFingerprint {
        kernel_release,
        kernel_version,
        arch: std::env::consts::ARCH.to_owned(),
        cpu_model,
        sockets,
        cores_per_socket,
        microcode,
        governor,
        swap_total_kib,
    })
}

fn read_evidence(path: &str) -> Result<String> {
    fs::read_to_string(path).map_err(|error| PerfError::harness(format!("{path}: {error}")))
}

// ---------------------------------------------------------------------------
// Benchmark manifest schema
// ---------------------------------------------------------------------------

/// The benchmark manifest (`perf/benchmarks.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkManifest {
    /// Schema version.
    pub schema: u32,
    /// Benchmarks, one or more per slice.
    #[serde(rename = "benchmark")]
    pub benchmarks: Vec<Benchmark>,
}

impl BenchmarkManifest {
    /// Finds the first benchmark owned by `slice` (case-insensitive).
    #[must_use]
    pub fn find_slice(&self, slice: &str) -> Option<&Benchmark> {
        let needle = slice.to_ascii_lowercase();
        self.benchmarks
            .iter()
            .find(|entry| entry.slice.to_ascii_lowercase() == needle)
    }
}

/// One benchmark entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Benchmark {
    /// Stable benchmark id.
    pub id: String,
    /// Owning slice id (`s0`…`s11`).
    pub slice: String,
    /// Input path relative to the materialized suite root.
    pub input: String,
    /// Measured facets.
    pub facets: Vec<String>,
    /// Measured backends.
    pub backends: Vec<String>,
    /// Baseline-artifact path.
    pub expected: String,
    /// Whether this slice is selected for the budget gate.
    #[serde(default)]
    pub selected: bool,
    /// Independent runs per measurement.
    #[serde(default = "default_repeats")]
    pub repeats: u32,
    /// Per-run timeout in milliseconds.
    pub timeout_ms: u64,
}

const fn default_repeats() -> u32 {
    DEFAULT_REPEATS
}

// ---------------------------------------------------------------------------
// Budget policy schema
// ---------------------------------------------------------------------------

/// The budget policy (`perf/budgets.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    /// Schema version.
    pub schema: u32,
    /// Wall-time ratio thresholds (candidate/base).
    pub wall_ratio: WallRatioPolicy,
    /// Peak RSS budgets.
    pub rss: RssPolicy,
    /// Release scorecard comparator.
    pub release: ReleasePolicy,
}

/// Wall-time ratio thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WallRatioPolicy {
    /// p50 ratio ceiling.
    pub p50: f64,
    /// p95 ratio ceiling.
    pub p95: f64,
    /// p99 ratio ceiling.
    pub p99: f64,
}

/// Peak RSS budgets: `C <= B + max(abs, rel * B)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssPolicy {
    /// p50 absolute floor (bytes).
    pub p50_abs_bytes: u64,
    /// p50 relative fraction.
    pub p50_rel: f64,
    /// p95 absolute floor (bytes).
    pub p95_abs_bytes: u64,
    /// p95 relative fraction.
    pub p95_rel: f64,
}

/// Artifact-byte budgets: `C <= B + max(abs, rel * B)`.
///
/// Not currently wired into [`BudgetPolicy`] or [`evaluate_budgets`]: the
/// harness does not yet measure emitted artifact size, so the budget is
/// removed from the policy to avoid advertising a gate it cannot perform.
/// Retained as a pub type so downstream re-exports remain valid until the
/// measurement lands and it can be re-wired.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPolicy {
    /// p50 absolute floor (bytes).
    pub p50_abs_bytes: u64,
    /// p50 relative fraction.
    pub p50_rel: f64,
}

/// Release scorecard comparator vs official upstream TypeScript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePolicy {
    /// Upstream comparator id, for example `typescript@7.0.2`.
    pub comparator: String,
    /// p95 wall ratio ceiling vs upstream.
    pub p95_ratio: f64,
    /// Geomean ratio ceiling vs upstream.
    pub geomean_ratio: f64,
}

// ---------------------------------------------------------------------------
// Measured result
// ---------------------------------------------------------------------------

/// Nearest-rank quantiles for one metric.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantiles {
    /// 50th percentile.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
}

impl Quantiles {
    /// All-zero placeholder quantiles.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
        }
    }

    /// Computes nearest-rank quantiles over `samples`.
    #[must_use]
    pub fn from_samples(samples: &[f64]) -> Self {
        Self {
            p50: nearest_rank(samples, 0.50),
            p95: nearest_rank(samples, 0.95),
            p99: nearest_rank(samples, 0.99),
        }
    }
}

/// Nearest-rank quantile over an unsorted sample slice.
#[must_use]
pub fn nearest_rank(samples: &[f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = sorted.len();
    let rank = (q * count as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(count - 1);
    sorted[index]
}

/// A baseline embedded in a measured result for comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Source path or comparator id.
    pub source: String,
    /// Base total wall time (ms) quantiles.
    pub wall_ms: Quantiles,
    /// Base peak RSS (bytes) quantiles.
    pub rss_bytes: Quantiles,
    /// Optional release comparator baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseBaseline>,
}

/// A release-comparator baseline (candidate vs upstream TypeScript).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseBaseline {
    /// Upstream comparator id.
    pub comparator: String,
    /// Candidate/upstream wall p95 ratio.
    pub wall_p95_ratio: f64,
    /// Candidate/upstream geomean ratio.
    pub geomean_ratio: f64,
}

/// The measurement result written by `measure` and read by `compare`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasureResult {
    /// Schema version.
    pub schema: u32,
    /// Host id.
    pub host: String,
    /// Measured slice.
    pub slice: String,
    /// Benchmark id.
    pub benchmark_id: String,
    /// Host identity captured at measurement (re-checked by `compare`).
    pub fingerprint: HostFingerprint,
    /// Expected runtime conditions (re-checked by `compare`).
    pub conditions_expected: HostConditions,
    /// Live conditions observed at measurement.
    pub conditions_observed: ObservedConditions,
    /// Whether observed conditions satisfied the manifest preconditions.
    pub conditions_match: bool,
    /// Independent runs performed.
    pub repeats: u32,
    /// Phase wall-time quantiles (ms) keyed by [`PHASE_KEYS`].
    pub phases: BTreeMap<String, Quantiles>,
    /// Peak RSS (bytes) quantiles.
    pub rss_bytes: Quantiles,
    /// Optional baseline for ratio comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    /// Whether this slice is selected for the budget gate.
    #[serde(default)]
    pub selected: bool,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Loads and validates a host manifest.
///
/// # Errors
/// Returns [`PerfErrorCode::HarnessError`] on read, parse, or schema failure.
pub fn load_host(path: &Path) -> Result<HostManifest> {
    let manifest: HostManifest = load_toml(path)?;
    require_schema(path, manifest.schema)?;
    Ok(manifest)
}

/// Loads and validates a benchmark manifest.
///
/// # Errors
/// Returns [`PerfErrorCode::HarnessError`] on read, parse, or schema failure.
pub fn load_manifest(path: &Path) -> Result<BenchmarkManifest> {
    let manifest: BenchmarkManifest = load_toml(path)?;
    require_schema(path, manifest.schema)?;
    Ok(manifest)
}

/// Loads and validates a budget policy.
///
/// # Errors
/// Returns [`PerfErrorCode::HarnessError`] on read, parse, or schema failure.
pub fn load_policy(path: &Path) -> Result<BudgetPolicy> {
    let policy: BudgetPolicy = load_toml(path)?;
    require_schema(path, policy.schema)?;
    Ok(policy)
}

fn load_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))?;
    toml::from_str(&text)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))
}

fn require_schema(path: &Path, schema: u32) -> Result<()> {
    if schema == PERF_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(PerfError::harness(format!(
            "{}: unsupported schema {schema}, expected {PERF_SCHEMA_VERSION}",
            path.display()
        )))
    }
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Options for [`measure`].
#[derive(Debug, Clone)]
pub struct MeasureOptions {
    /// Path to the host manifest.
    pub host_path: PathBuf,
    /// Path to the benchmark manifest.
    pub manifest_path: PathBuf,
    /// Slice id to measure.
    pub slice: String,
    /// Output result path.
    pub out_path: PathBuf,
    /// Optional baseline artifact path.
    pub baseline_path: Option<PathBuf>,
    /// Workspace root (for the suite seam).
    pub workspace_root: PathBuf,
    /// Materialized suite root (for the suite seam).
    pub snapshot_root: PathBuf,
}

/// Runs the requested slice's benchmark and writes a [`MeasureResult`].
///
/// # Errors
/// - [`PerfErrorCode::InvalidHost`] if the machine does not match the host.
/// - [`PerfErrorCode::NoBaseline`] if a baseline is requested but missing.
/// - [`PerfErrorCode::HarnessError`] on load/seam/serialization failure.
pub fn measure(options: &MeasureOptions) -> Result<MeasureResult> {
    let host = load_host(&options.host_path)?;
    let machine = read_machine_fingerprint()?;
    host.require_match(&machine)?;

    let manifest = load_manifest(&options.manifest_path)?;
    let benchmark = manifest
        .find_slice(&options.slice)
        .ok_or_else(|| PerfError::harness(format!("no benchmark for slice `{}`", options.slice)))?;

    let baseline = load_baseline(options.baseline_path.as_deref())?;

    // S0 seam: drive the suite for the slice `repeats` times, timing the whole
    // seam wall as `total` and aggregating real frontend phase telemetry from
    // every check-backend compile the runner performs.
    let repeats = benchmark.repeats.max(1);
    let mut wall_samples = Vec::with_capacity(repeats as usize);
    let mut scan_samples = Vec::with_capacity(repeats as usize);
    let mut parse_samples = Vec::with_capacity(repeats as usize);
    let mut bind_samples = Vec::with_capacity(repeats as usize);
    let mut check_samples = Vec::with_capacity(repeats as usize);
    let mut emit_samples = Vec::with_capacity(repeats as usize);
    let mut rss_samples = Vec::with_capacity(repeats as usize);
    // Reset the peak RSS high-water mark before every repeat so each sample
    // measures only that run's allocation peak, not the cumulative peak of
    // every prior repeat and the harness.
    for _ in 0..repeats {
        reset_peak_rss()?;
        let started = Instant::now();
        let (_report, telemetry) = run_suite_with_telemetry(
            &options.workspace_root,
            &options.snapshot_root,
            &RunFilterOptions {
                status: StatusFilter::All,
                slice: Some(options.slice.clone()),
                backends: BackendFilter::All,
                shards: None,
            },
        )
        .map_err(|error| PerfError::harness(format!("run_suite seam failed: {error}")))?;
        wall_samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        scan_samples.push(telemetry.millis(Phase::Scan));
        parse_samples.push(telemetry.millis(Phase::Parse));
        bind_samples.push(telemetry.millis(Phase::Bind));
        check_samples.push(telemetry.millis(Phase::Check));
        emit_samples.push(telemetry.millis(Phase::Emit));
        rss_samples.push(read_peak_rss_bytes()? as f64);
    }

    // `total` is the whole measured seam wall; the phase split is the compiler's
    // own scan/parse/bind/check/emit telemetry. `scan` is emitted as an optional
    // key alongside the five required keys; zero-duration phases stay 0.0.
    let mut phases = BTreeMap::new();
    phases.insert("scan".to_owned(), Quantiles::from_samples(&scan_samples));
    phases.insert("parse".to_owned(), Quantiles::from_samples(&parse_samples));
    phases.insert("bind".to_owned(), Quantiles::from_samples(&bind_samples));
    phases.insert("check".to_owned(), Quantiles::from_samples(&check_samples));
    phases.insert("emit".to_owned(), Quantiles::from_samples(&emit_samples));
    phases.insert("total".to_owned(), Quantiles::from_samples(&wall_samples));
    require_phase_keys(&phases)?;

    let rss_bytes = Quantiles::from_samples(&rss_samples);

    let conditions_observed = ObservedConditions {
        governor: machine.governor.clone(),
        swap_total_kib: machine.swap_total_kib,
    };
    let conditions_match = conditions_observed.satisfies(&host.conditions);

    let result = MeasureResult {
        schema: PERF_SCHEMA_VERSION,
        host: host.host,
        slice: options.slice.clone(),
        benchmark_id: benchmark.id.clone(),
        fingerprint: host.fingerprint,
        conditions_expected: host.conditions,
        conditions_observed,
        conditions_match,
        repeats,
        phases,
        rss_bytes,
        baseline,
        selected: benchmark.selected,
    };

    write_result(&options.out_path, &result)?;
    Ok(result)
}

/// Ensures a measured phase map carries every required phase key.
///
/// A successful run whose result omits any of [`PHASE_KEYS`] is a
/// [`PerfErrorCode::HarnessError`]: the backend produced a result the budget
/// comparator cannot evaluate.
fn require_phase_keys(phases: &BTreeMap<String, Quantiles>) -> Result<()> {
    for key in PHASE_KEYS {
        if !phases.contains_key(key) {
            return Err(PerfError::harness(format!(
                "measured result is missing required phase key `{key}`"
            )));
        }
    }
    Ok(())
}

fn load_baseline(path: Option<&Path>) -> Result<Option<Baseline>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Err(PerfError::new(
            PerfErrorCode::NoBaseline,
            format!("baseline not found: {}", path.display()),
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))?;
    let base: MeasureResult = serde_json::from_str(&text)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))?;
    let wall_ms = base.phases.get("total").copied().ok_or_else(|| {
        PerfError::new(
            PerfErrorCode::NoBaseline,
            format!("{}: baseline has no `total` phase", path.display()),
        )
    })?;
    Ok(Some(Baseline {
        source: path.display().to_string(),
        wall_ms,
        rss_bytes: base.rss_bytes,
        release: None,
    }))
}

/// Resets the process peak RSS high-water mark (`VmHWM`) so each benchmark
/// repeat measures its own allocation peak.
///
/// On Linux this writes `5` to `/proc/self/clear_refs`, which the kernel
/// documents as resetting the high-water mark to the current resident set
/// size. On other platforms this returns a contextual error instead of
/// silently producing fake data.
fn reset_peak_rss() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        fs::write("/proc/self/clear_refs", "5")
            .map_err(|error| PerfError::harness(format!("/proc/self/clear_refs: {error}")))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(PerfError::harness(
            "peak RSS reset requires /proc/self/clear_refs and is only available on Linux",
        ))
    }
}

/// Reads the current process peak RSS high-water mark (`VmHWM`) in bytes.
///
/// After a [`reset_peak_rss`] call this is the peak since that reset, so the
/// benchmark can attribute peak memory to each measured repeat. On non-Linux
/// hosts this returns a contextual error instead of a fake zero.
fn read_peak_rss_bytes() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let content = fs::read_to_string("/proc/self/status")
            .map_err(|error| PerfError::harness(format!("/proc/self/status: {error}")))?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kib: u64 = rest
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| PerfError::harness("/proc/self/status: VmHWM has no value"))?
                    .parse()
                    .map_err(|error| {
                        PerfError::harness(format!(
                            "/proc/self/status: VmHWM is not a number: {error}"
                        ))
                    })?;
                return Ok(kib * 1024);
            }
        }
        Err(PerfError::harness(
            "/proc/self/status: VmHWM field not found",
        ))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(PerfError::harness(
            "peak RSS read requires /proc/self/status and is only available on Linux",
        ))
    }
}

fn write_result(path: &Path, result: &MeasureResult) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| PerfError::harness(format!("{}: {error}", parent.display())))?;
    }
    let json = serde_json::to_string_pretty(result)
        .map_err(|error| PerfError::harness(format!("serialize result: {error}")))?;
    fs::write(path, json)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Loads a measured result JSON.
///
/// # Errors
/// Returns [`PerfErrorCode::HarnessError`] on read or parse failure.
pub fn load_result(path: &Path) -> Result<MeasureResult> {
    let text = fs::read_to_string(path)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|error| PerfError::harness(format!("{}: {error}", path.display())))
}

/// Evaluates a measured result against the budget policy, re-checking that the
/// live machine still matches the result's host identity.
///
/// # Errors
/// - [`PerfErrorCode::InvalidHost`] if the machine no longer matches.
/// - [`PerfErrorCode::NoBaseline`] if the slice is selected but the result has no baseline.
/// - [`PerfErrorCode::BudgetBreach`] if any threshold is exceeded.
pub fn compare(
    result: &MeasureResult,
    policy: &BudgetPolicy,
    machine: &MachineFingerprint,
) -> Result<()> {
    require_no_host_mismatches(result.fingerprint.identity_mismatches(machine))?;
    evaluate_budgets(result, policy)
}

/// Evaluates only the budget thresholds (host identity assumed checked).
///
/// # Errors
/// - [`PerfErrorCode::NoBaseline`] if the slice is selected but the result has no baseline.
/// - [`PerfErrorCode::BudgetBreach`] naming the first exceeded threshold.
pub fn evaluate_budgets(result: &MeasureResult, policy: &BudgetPolicy) -> Result<()> {
    if result.selected && result.baseline.is_none() {
        return Err(PerfError::new(
            PerfErrorCode::NoBaseline,
            format!(
                "selected slice `{}` (benchmark `{}`) is missing a baseline for the budget gate",
                result.slice, result.benchmark_id
            ),
        ));
    }

    let Some(baseline) = &result.baseline else {
        // Unselected informational slice: nothing to ratio-gate.
        return Ok(());
    };

    let candidate_wall = result
        .phases
        .get("total")
        .copied()
        .unwrap_or_else(Quantiles::zero);

    check_ratio(
        "wall.p50",
        candidate_wall.p50,
        baseline.wall_ms.p50,
        policy.wall_ratio.p50,
    )?;
    check_ratio(
        "wall.p95",
        candidate_wall.p95,
        baseline.wall_ms.p95,
        policy.wall_ratio.p95,
    )?;
    check_ratio(
        "wall.p99",
        candidate_wall.p99,
        baseline.wall_ms.p99,
        policy.wall_ratio.p99,
    )?;

    check_abs_rel(
        "rss.p50",
        result.rss_bytes.p50,
        baseline.rss_bytes.p50,
        policy.rss.p50_abs_bytes as f64,
        policy.rss.p50_rel,
    )?;
    check_abs_rel(
        "rss.p95",
        result.rss_bytes.p95,
        baseline.rss_bytes.p95,
        policy.rss.p95_abs_bytes as f64,
        policy.rss.p95_rel,
    )?;

    if let Some(release) = &baseline.release {
        if release.wall_p95_ratio > policy.release.p95_ratio {
            return Err(budget_breach(
                "release.p95",
                release.wall_p95_ratio,
                policy.release.p95_ratio,
            ));
        }
        if release.geomean_ratio > policy.release.geomean_ratio {
            return Err(budget_breach(
                "release.geomean",
                release.geomean_ratio,
                policy.release.geomean_ratio,
            ));
        }
    }

    Ok(())
}

fn check_ratio(name: &str, candidate: f64, base: f64, ceiling: f64) -> Result<()> {
    if base <= 0.0 {
        return Ok(());
    }
    let ratio = candidate / base;
    if ratio > ceiling {
        Err(budget_breach(name, ratio, ceiling))
    } else {
        Ok(())
    }
}

fn check_abs_rel(name: &str, candidate: f64, base: f64, abs_floor: f64, rel: f64) -> Result<()> {
    let allowance = abs_floor.max(rel * base);
    let limit = base + allowance;
    if candidate > limit {
        Err(PerfError::new(
            PerfErrorCode::BudgetBreach,
            format!(
                "{name}: candidate {candidate} exceeds limit {limit} (base {base} + max({abs_floor}, {rel}*base))"
            ),
        ))
    } else {
        Ok(())
    }
}

fn budget_breach(name: &str, ratio: f64, ceiling: f64) -> PerfError {
    PerfError::new(
        PerfErrorCode::BudgetBreach,
        format!("{name}: ratio {ratio:.4} exceeds ceiling {ceiling:.4}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bh1_toml() -> &'static str {
        include_str!("../../../perf/hosts/bh1.toml")
    }

    fn sample_machine() -> MachineFingerprint {
        MachineFingerprint {
            kernel_release: "7.0.0-28-generic".to_owned(),
            kernel_version: "#28-Ubuntu SMP PREEMPT_DYNAMIC Sun Jun 21 01:01:36 UTC 2026"
                .to_owned(),
            arch: "x86_64".to_owned(),
            cpu_model: "Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz".to_owned(),
            sockets: 2,
            cores_per_socket: 20,
            microcode: "0x2007006".to_owned(),
            governor: "performance".to_owned(),
            swap_total_kib: 0,
        }
    }

    fn budget_policy() -> BudgetPolicy {
        BudgetPolicy {
            schema: PERF_SCHEMA_VERSION,
            wall_ratio: WallRatioPolicy {
                p50: 1.05,
                p95: 1.10,
                p99: 1.15,
            },
            rss: RssPolicy {
                p50_abs_bytes: 16 << 20,
                p50_rel: 0.05,
                p95_abs_bytes: 32 << 20,
                p95_rel: 0.10,
            },
            release: ReleasePolicy {
                comparator: "typescript@7.0.2".to_owned(),
                p95_ratio: 1.25,
                geomean_ratio: 1.00,
            },
        }
    }

    fn result_with_baseline(base: Quantiles, candidate: Quantiles) -> MeasureResult {
        let mut phases = BTreeMap::new();
        for key in PHASE_KEYS {
            let value = if key == "total" {
                candidate
            } else {
                Quantiles::zero()
            };
            phases.insert(key.to_owned(), value);
        }
        MeasureResult {
            schema: PERF_SCHEMA_VERSION,
            host: "BH1".to_owned(),
            slice: "s0".to_owned(),
            benchmark_id: "s0-conformance-foundation".to_owned(),
            fingerprint: host_fingerprint_from_machine(&sample_machine()),
            conditions_expected: HostConditions {
                governor: "performance".to_owned(),
                swap_total_kib: 0,
                numa_node0_cpus: "0-19".to_owned(),
            },
            conditions_observed: ObservedConditions {
                governor: "performance".to_owned(),
                swap_total_kib: 0,
            },
            conditions_match: true,
            repeats: 3,
            phases,
            rss_bytes: Quantiles {
                p50: 100_000_000.0,
                p95: 100_000_000.0,
                p99: 100_000_000.0,
            },
            baseline: Some(Baseline {
                source: "perf/baselines/s0.json".to_owned(),
                wall_ms: base,
                rss_bytes: Quantiles {
                    p50: 100_000_000.0,
                    p95: 100_000_000.0,
                    p99: 100_000_000.0,
                },
                release: None,
            }),
            selected: false,
        }
    }

    fn host_fingerprint_from_machine(machine: &MachineFingerprint) -> HostFingerprint {
        HostFingerprint {
            kernel_release: machine.kernel_release.clone(),
            kernel_version: "#28-Ubuntu SMP PREEMPT_DYNAMIC".to_owned(),
            arch: machine.arch.clone(),
            cpu_model: machine.cpu_model.clone(),
            sockets: machine.sockets,
            cores_per_socket: machine.cores_per_socket,
            microcode: machine.microcode.clone(),
        }
    }

    #[test]
    fn host_read_parses_committed_manifest() {
        let host = toml::from_str::<HostManifest>(bh1_toml()).expect("parse bh1.toml");
        assert_eq!(host.schema, PERF_SCHEMA_VERSION);
        assert_eq!(host.host, "BH1");
        assert_eq!(host.fingerprint.kernel_release, "7.0.0-28-generic");
        assert_eq!(host.fingerprint.arch, "x86_64");
        assert_eq!(
            host.fingerprint.cpu_model,
            "Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz"
        );
        assert_eq!(host.fingerprint.sockets, 2);
        assert_eq!(host.fingerprint.cores_per_socket, 20);
        assert_eq!(host.conditions.governor, "performance");
        assert_eq!(host.conditions.swap_total_kib, 0);
        assert_eq!(
            host.source.get("governor").map(String::as_str),
            Some("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_read_reads_live_proc_and_sys_evidence() {
        let machine = read_machine_fingerprint().expect("read live host evidence");
        assert!(!machine.kernel_release.is_empty());
        assert!(!machine.cpu_model.is_empty());
        assert!(machine.sockets >= 1);
        assert!(machine.cores_per_socket >= 1);
        assert!(!machine.governor.is_empty());
    }

    #[test]
    fn fingerprint_matches_identical_identity() {
        let machine = sample_machine();
        let fingerprint = host_fingerprint_from_machine(&machine);
        assert!(fingerprint.identity_mismatches(&machine).is_empty());
        assert!(fingerprint.require_match(&machine).is_ok());
    }

    #[test]
    fn fingerprint_mismatch_is_invalid_host() {
        let machine = sample_machine();
        let mut fingerprint = host_fingerprint_from_machine(&machine);
        fingerprint.cpu_model = "Intel(R) Xeon(R) Gold 9999 CPU @ 9.00GHz".to_owned();
        let diffs = fingerprint.identity_mismatches(&machine);
        assert_eq!(diffs, vec!["cpu_model".to_owned()]);
        let error = fingerprint.require_match(&machine).unwrap_err();
        assert_eq!(error.code, PerfErrorCode::InvalidHost);
    }

    #[test]
    fn runtime_condition_mismatch_is_advisory_not_invalid_host() {
        let machine = sample_machine();
        let fingerprint = host_fingerprint_from_machine(&machine);
        // Identity still matches even when governor/swap differ.
        assert!(fingerprint.require_match(&machine).is_ok());
        let untuned = HostConditions {
            governor: "powersave".to_owned(),
            swap_total_kib: 4096,
            numa_node0_cpus: "0-19".to_owned(),
        };
        let observed = ObservedConditions {
            governor: machine.governor.clone(),
            swap_total_kib: machine.swap_total_kib,
        };
        // Observed (performance/0) does not satisfy an untuned expectation.
        assert!(!observed.satisfies(&untuned));
        // But a matching expectation is satisfied.
        let tuned = HostConditions {
            governor: "performance".to_owned(),
            swap_total_kib: 0,
            numa_node0_cpus: "0-19".to_owned(),
        };
        assert!(observed.satisfies(&tuned));
    }

    #[test]
    fn compare_passes_when_within_budget() {
        let base = Quantiles {
            p50: 100.0,
            p95: 100.0,
            p99: 100.0,
        };
        let candidate = Quantiles {
            p50: 103.0,
            p95: 108.0,
            p99: 110.0,
        };
        let result = result_with_baseline(base, candidate);
        assert!(evaluate_budgets(&result, &budget_policy()).is_ok());
        assert!(compare(&result, &budget_policy(), &sample_machine()).is_ok());
    }

    #[test]
    fn compare_fails_on_wall_ratio_breach() {
        let base = Quantiles {
            p50: 100.0,
            p95: 100.0,
            p99: 100.0,
        };
        let candidate = Quantiles {
            p50: 100.0,
            p95: 150.0,
            p99: 100.0,
        };
        let result = result_with_baseline(base, candidate);
        let error = evaluate_budgets(&result, &budget_policy()).unwrap_err();
        assert_eq!(error.code, PerfErrorCode::BudgetBreach);
        assert!(error.detail.contains("wall.p95"));
    }

    #[test]
    fn compare_without_baseline_passes() {
        let base = Quantiles::zero();
        let candidate = Quantiles::zero();
        let mut result = result_with_baseline(base, candidate);
        result.baseline = None;
        assert!(evaluate_budgets(&result, &budget_policy()).is_ok());
    }

    #[test]
    fn perf_budget_rejects_slice_without_baseline() {
        let manifest: BenchmarkManifest =
            toml::from_str(include_str!("../../../perf/benchmarks.toml"))
                .expect("parse benchmarks fixture");
        let s11 = manifest
            .find_slice("s11")
            .expect("s11 placeholder fixture must exist");
        assert!(
            s11.selected,
            "s11 placeholder must be selected for the budget gate"
        );

        let mut phases = BTreeMap::new();
        for key in PHASE_KEYS {
            phases.insert(key.to_owned(), Quantiles::zero());
        }

        let result = MeasureResult {
            schema: PERF_SCHEMA_VERSION,
            host: "BH1".to_owned(),
            slice: s11.slice.clone(),
            benchmark_id: s11.id.clone(),
            fingerprint: host_fingerprint_from_machine(&sample_machine()),
            conditions_expected: HostConditions {
                governor: "performance".to_owned(),
                swap_total_kib: 0,
                numa_node0_cpus: "0-19".to_owned(),
            },
            conditions_observed: ObservedConditions {
                governor: "performance".to_owned(),
                swap_total_kib: 0,
            },
            conditions_match: true,
            repeats: 3,
            phases,
            rss_bytes: Quantiles::zero(),
            baseline: None,
            selected: true,
        };

        let error = compare(&result, &budget_policy(), &sample_machine()).expect_err(
            "selected slice without baseline must fail the budget gate",
        );
        assert_eq!(error.code, PerfErrorCode::NoBaseline);
        assert_eq!(error.code.exit_code(), 4);
        assert!(error.detail.contains("s11"), "error must name the slice");
        assert!(
            error.detail.contains("s11-language-service"),
            "error must name the benchmark"
        );
        assert!(
            error.detail.contains("baseline"),
            "error must name the missing baseline"
        );
    }

    #[test]
    fn compare_detects_stale_host_identity() {
        let base = Quantiles::zero();
        let result = result_with_baseline(base, base);
        let mut machine = sample_machine();
        machine.cpu_model = "Different CPU".to_owned();
        let error = compare(&result, &budget_policy(), &machine).unwrap_err();
        assert_eq!(error.code, PerfErrorCode::InvalidHost);
    }

    #[test]
    fn parse_proc_version_extracts_release_and_tail() {
        let content = "Linux version 7.0.0-28-generic (buildd@lcy02) (gcc 15) #28-Ubuntu SMP PREEMPT_DYNAMIC Sun Jun 21 01:01:36 UTC 2026\n";
        let (release, version) = parse_proc_version(content).unwrap();
        assert_eq!(release, "7.0.0-28-generic");
        assert!(version.starts_with("#28-Ubuntu SMP PREEMPT_DYNAMIC"));
    }

    #[test]
    fn parse_proc_cpuinfo_counts_sockets_and_cores() {
        let content = "model name\t: Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz\nmicrocode\t: 0x2007006\ncpu cores\t: 20\nphysical id\t: 0\n\nphysical id\t: 1\ncpu cores\t: 20\n";
        let (model, sockets, cores, micro) = parse_proc_cpuinfo(content).unwrap();
        assert_eq!(model, "Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz");
        assert_eq!(sockets, 2);
        assert_eq!(cores, 20);
        assert_eq!(micro, "0x2007006");
    }

    #[test]
    fn parse_proc_meminfo_reads_swap_total() {
        let content = "MemTotal:  791180784 kB\nSwapTotal:      0 kB\n";
        assert_eq!(parse_proc_meminfo_swap(content).unwrap(), 0);
    }

    #[test]
    fn nearest_rank_uses_ceil_rank() {
        let samples = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(nearest_rank(&samples, 0.50), 20.0);
        assert_eq!(nearest_rank(&samples, 0.95), 40.0);
        assert_eq!(nearest_rank(&[], 0.5), 0.0);
    }

    #[test]
    fn measured_result_carries_all_required_phase_keys_with_positive_total() {
        let candidate = Quantiles {
            p50: 12.5,
            p95: 18.0,
            p99: 20.0,
        };
        let result = result_with_baseline(Quantiles::zero(), candidate);
        // Every required phase key is present in the measured result.
        for key in PHASE_KEYS {
            assert!(
                result.phases.contains_key(key),
                "measured result must carry phase key `{key}`"
            );
        }
        // The gate accepts a complete result, and total wall is positive.
        require_phase_keys(&result.phases).expect("complete phase map passes the gate");
        assert!(result.phases["total"].p50 > 0.0, "total wall is positive");
    }

    #[test]
    fn require_phase_keys_rejects_a_missing_key_as_harness_error() {
        let mut result = result_with_baseline(Quantiles::zero(), Quantiles::zero());
        result.phases.remove("check");
        let error = require_phase_keys(&result.phases)
            .expect_err("a result missing `check` must be rejected");
        assert_eq!(error.code, PerfErrorCode::HarnessError);
        assert_eq!(error.code.exit_code(), 6);
        assert!(error.detail.contains("check"));
    }

    #[test]
    fn load_baseline_rejects_missing_total_phase() {
        // A baseline JSON that parsed successfully but has no `total` phase key
        // must fail loudly with NoBaseline, not silently fall back to zero and
        // pass every wall-time budget.
        let mut result = result_with_baseline(Quantiles::zero(), Quantiles::zero());
        result.phases.remove("total");
        let json = serde_json::to_string_pretty(&result).expect("serialize baseline");
        let dir = std::env::temp_dir();
        let path = dir.join("bamts_perf_baseline_no_total.json");
        std::fs::write(&path, &json).expect("write temp baseline");
        let error = load_baseline(Some(&path))
            .expect_err("baseline missing `total` must be rejected")
            .code;
        assert_eq!(error, PerfErrorCode::NoBaseline);
        assert_eq!(error.exit_code(), 4);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "linux")]
    fn parse_proc_status_value(rest: &str, field: &str) -> Result<u64> {
        let kib: u64 = rest
            .split_whitespace()
            .next()
            .ok_or_else(|| PerfError::harness(format!("/proc/self/status: {field} has no value")))?
            .parse()
            .map_err(|error| {
                PerfError::harness(format!(
                    "/proc/self/status: {field} is not a number: {error}"
                ))
            })?;
        Ok(kib * 1024)
    }

    #[cfg(target_os = "linux")]
    /// Reads the current `(VmHWM, VmRSS)` pair from a single `/proc/self/status`
    /// snapshot so the test compares the two fields atomically.
    fn read_hwm_and_rss() -> Result<(u64, u64)> {
        let content = std::fs::read_to_string("/proc/self/status")
            .map_err(|error| PerfError::harness(format!("/proc/self/status: {error}")))?;
        let mut hwm = None;
        let mut rss = None;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                hwm = Some(parse_proc_status_value(rest, "VmHWM")?);
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss = Some(parse_proc_status_value(rest, "VmRSS")?);
            }
        }
        match (hwm, rss) {
            (Some(h), Some(r)) => Ok((h, r)),
            (None, _) => Err(PerfError::harness("/proc/self/status: VmHWM not found")),
            (_, None) => Err(PerfError::harness("/proc/self/status: VmRSS not found")),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reset_peak_rss_resets_hwm_to_current_rss() {
        // Raise the high-water mark by faulting in a large allocation.
        let (hwm_before, _rss_before) =
            read_hwm_and_rss().expect("read /proc/self/status before allocation");
        let big = {
            let mut v = vec![0u8; 64 * 1024 * 1024];
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            std::hint::black_box(v)
        };
        let (hwm_during, _rss_during) =
            read_hwm_and_rss().expect("read /proc/self/status during allocation");
        assert!(hwm_during >= hwm_before, "allocation must not lower VmHWM");

        // Dropping the allocation returns the mapped pages to the OS on glibc,
        // lowering current RSS while leaving the lifetime HWM high.
        drop(big);

        reset_peak_rss().expect("reset VmHWM");

        let (hwm_after, rss_after) =
            read_hwm_and_rss().expect("read /proc/self/status after reset");
        assert!(
            hwm_after > 0,
            "VmHWM after reset must be positive, not fake"
        );
        assert!(
            hwm_after >= rss_after,
            "VmHWM cannot be below current VmRSS (hwm={hwm_after}, rss={rss_after})"
        );
        assert!(
            hwm_after <= rss_after + 8 * 1024 * 1024,
            "VmHWM after reset must be near current VmRSS (within test overhead); \
             got hwm={hwm_after}, rss={rss_after}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn peak_rss_sampling_is_unsupported_outside_linux() {
        // Resetting and reading VmHWM are platform-specific; on non-Linux they
        // must fail with a typed, contextual error rather than return fake data.
        let reset =
            reset_peak_rss().expect_err("reset should fail on non-Linux with a contextual error");
        assert_eq!(reset.code, PerfErrorCode::HarnessError);
        assert!(
            reset.detail.contains("Linux"),
            "reset error must name the platform"
        );

        let read = read_peak_rss_bytes()
            .expect_err("read should fail on non-Linux with a contextual error");
        assert_eq!(read.code, PerfErrorCode::HarnessError);
        assert!(
            read.detail.contains("Linux"),
            "read error must name the platform"
        );
    }
}
