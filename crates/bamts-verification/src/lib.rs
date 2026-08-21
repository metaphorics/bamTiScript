pub mod check_cells;
pub mod corpus;
pub mod facets;
pub mod fixtures;
pub mod formal_bridge;
pub mod formal_gates;
pub mod ledger;
pub mod oracle_pins;
pub mod perf;
pub mod suite;
pub mod ts_ledger;
pub mod workspace_guard;

pub use facets::{
    DiagnosticCodeMap, FacetDiagnostic, FacetVerdict, VerifiedJsOutcome, compare_diagnostics,
    compare_dts, compare_js, compare_js_cases, compare_symbols, compare_types,
    load_diagnostic_code_map, parse_diagnostic_code_map,
};
pub use fixtures::{
    FixtureVerification, MaterializedFixture, TreeHash, generate_boundary, hash_file, hash_tree,
    materialize_fixtures, verify_fixtures,
};
pub use oracle_pins::{OraclePins, verify_oracle_pins};
pub use perf::{
    ArtifactPolicy, Baseline, Benchmark, BenchmarkManifest, BudgetPolicy, Fixture, FixtureGroup,
    FixtureOrigin, FixtureScore, HostConditions, HostFingerprint, HostManifest, MachineFingerprint,
    MeasureOptions, MeasureResult, ObservedConditions, PerfError, PerfErrorCode, Quantiles,
    ReleaseBaseline, ReleasePolicy, RssPolicy, Scorecard, ScorecardOptions, WallRatioPolicy,
    bless_baseline, capture_scorecard, check_baseline, compare as compare_perf, evaluate_budgets,
    load_host, load_manifest as load_perf_manifest, load_policy, load_result, load_scorecard,
    measure, nearest_rank, read_machine_fingerprint, validate_scorecard,
};
pub use suite::{
    AssetKind, BackendFilter, CellResult, CiMode, CiOptions, FailureClass, IndexEntry,
    RunFilterOptions, RunState, StatusFilter, SuiteIndex, SuiteRunReport, SuiteSnapshot,
    SyncOptions, VerifiedSuite, audit_ledger, run_ci, run_suite, run_suite_with_telemetry,
    sync_suite, write_suite_ledger,
};
pub use ts_ledger::{TsLedger, TsLedgerReader, TsLedgerWriter};

use std::fmt;

pub type Result<T, E = VerificationError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gate {
    G0,
    G1,
    G2,
    G3,
    G4,
    G5,
    G6,
}

impl Gate {
    pub const FORMAL_ORDER: [Self; 6] =
        [Self::G1, Self::G2, Self::G5, Self::G3, Self::G4, Self::G6];

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "G0" => Ok(Self::G0),
            "G1" => Ok(Self::G1),
            "G2" => Ok(Self::G2),
            "G3" => Ok(Self::G3),
            "G4" => Ok(Self::G4),
            "G5" => Ok(Self::G5),
            "G6" => Ok(Self::G6),
            _ => Err(VerificationError::new(
                ErrorCode::Usage,
                format!("unknown gate `{value}`"),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateReport {
    pub gate: Gate,
    pub checks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Usage,
    Io,
    Json,
    Toml,
    Schema,
    Digest,
    Duplicate,
    SetMismatch,
    Transition,
    Workspace,
    GateDependency,
    ToolMissing,
    ToolFailed,
    Replay,
    ProvenanceMismatch,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "E_USAGE",
            Self::Io => "E_IO",
            Self::Json => "E_JSON",
            Self::Toml => "E_TOML",
            Self::Schema => "E_SCHEMA",
            Self::Digest => "E_DIGEST",
            Self::Duplicate => "E_DUPLICATE",
            Self::SetMismatch => "E_SET_MISMATCH",
            Self::Transition => "E_TRANSITION",
            Self::Workspace => "E_WORKSPACE",
            Self::GateDependency => "E_GATE_DEPENDENCY",
            Self::ToolMissing => "E_TOOL_MISSING",
            Self::ToolFailed => "E_TOOL_FAILED",
            Self::Replay => "E_REPLAY",
            Self::ProvenanceMismatch => "PROVENANCE_MISMATCH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationError {
    code: ErrorCode,
    detail: String,
}

impl VerificationError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub const fn code(&self) -> ErrorCode {
        self.code
    }
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "bamts-verification: {}: {}",
            self.code.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for VerificationError {}
