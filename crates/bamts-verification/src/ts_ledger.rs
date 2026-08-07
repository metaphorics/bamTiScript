//! Deterministic, schema-bound TypeScript 7.0.2 conformance applicability ledger.
//!
//! This module owns the typed ledger consumed by the CI conformance harness.
//! The ledger is sorted by `(input, facet, id)`, rejects unknown object fields
//! at every boundary, and enforces the conditional rules from the Issue #12
//! decision:
//!
//! * `deferred` entries require a non-empty `blockedBy` list.
//! * `included` entries may only carry `PROMISED_COMPILER_CONTRACT` or
//!   `PROMISED_NODE_API_CONTRACT` as their reason code.
//! * every entry keeps the schema `evidence` minimum of one record.
//!
//! Serialization is stable: the same logical ledger always emits the same bytes
//! because every JSON object key is sorted lexicographically, the `entries` array
//! is sorted before writing, and line endings are LF with no trailing whitespace.

use std::{collections::BTreeSet, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    ErrorCode, Result, VerificationError,
    oracle_pins::{NPM_SPECIFIER, OraclePins},
};

/// Ledger schema version. Must equal 1.
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

/// Suite case root carried in the ledger oracle.
pub const SUITE_CASE_ROOT: &str = "tests/cases";
/// Suite baseline root carried in the ledger oracle.
pub const SUITE_BASELINE_ROOT: &str = "tests/baselines/reference";

/// The 17 upstream partition values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Partition {
    #[serde(rename = "compiler")]
    Compiler,
    #[serde(rename = "conformance")]
    Conformance,
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "projects")]
    Projects,
    #[serde(rename = "transpile")]
    Transpile,
    #[serde(rename = "unittests")]
    UnitTests,
    #[serde(rename = "api")]
    Api,
    #[serde(rename = "astnav")]
    AstNav,
    #[serde(rename = "config")]
    Config,
    #[serde(rename = "fourslash")]
    Fourslash,
    #[serde(rename = "lsp")]
    Lsp,
    #[serde(rename = "tsbuild")]
    TsBuild,
    #[serde(rename = "tsbuildWatch")]
    TsBuildWatch,
    #[serde(rename = "tsc")]
    Tsc,
    #[serde(rename = "tscWatch")]
    TscWatch,
    #[serde(rename = "tsoptions")]
    TsOptions,
    #[serde(rename = "other")]
    Other,
}

impl Partition {
    /// The JSON name used by the ledger and the shard-key sort.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compiler => "compiler",
            Self::Conformance => "conformance",
            Self::Project => "project",
            Self::Projects => "projects",
            Self::Transpile => "transpile",
            Self::UnitTests => "unittests",
            Self::Api => "api",
            Self::AstNav => "astnav",
            Self::Config => "config",
            Self::Fourslash => "fourslash",
            Self::Lsp => "lsp",
            Self::TsBuild => "tsbuild",
            Self::TsBuildWatch => "tsbuildWatch",
            Self::Tsc => "tsc",
            Self::TscWatch => "tscWatch",
            Self::TsOptions => "tsoptions",
            Self::Other => "other",
        }
    }
}

/// The 15 observed facet values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Facet {
    #[serde(rename = "parse")]
    Parse,
    #[serde(rename = "diagnostics")]
    Diagnostics,
    #[serde(rename = "types")]
    Types,
    #[serde(rename = "symbols")]
    Symbols,
    #[serde(rename = "jsEmit")]
    JsEmit,
    #[serde(rename = "dtsEmit")]
    DtsEmit,
    #[serde(rename = "moduleResolution")]
    ModuleResolution,
    #[serde(rename = "config")]
    Config,
    #[serde(rename = "cli")]
    Cli,
    #[serde(rename = "build")]
    Build,
    #[serde(rename = "watch")]
    Watch,
    #[serde(rename = "nodeApi")]
    NodeApi,
    #[serde(rename = "languageService")]
    LanguageService,
    #[serde(rename = "harness")]
    Harness,
    #[serde(rename = "implementation")]
    Implementation,
}

impl Facet {
    /// The JSON name used by the ledger and the sort key.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Diagnostics => "diagnostics",
            Self::Types => "types",
            Self::Symbols => "symbols",
            Self::JsEmit => "jsEmit",
            Self::DtsEmit => "dtsEmit",
            Self::ModuleResolution => "moduleResolution",
            Self::Config => "config",
            Self::Cli => "cli",
            Self::Build => "build",
            Self::Watch => "watch",
            Self::NodeApi => "nodeApi",
            Self::LanguageService => "languageService",
            Self::Harness => "harness",
            Self::Implementation => "implementation",
        }
    }
}

/// Status of a ledger cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Status {
    #[serde(rename = "included")]
    Included,
    #[serde(rename = "deferred")]
    Deferred,
    #[serde(rename = "excluded")]
    Excluded,
}

/// The 7 reason codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum ReasonCode {
    #[serde(rename = "PROMISED_COMPILER_CONTRACT")]
    PromisedCompilerContract,
    #[serde(rename = "PROMISED_NODE_API_CONTRACT")]
    PromisedNodeApiContract,
    #[serde(rename = "PROMISED_NOT_IMPLEMENTED")]
    PromisedNotImplemented,
    #[serde(rename = "LANGUAGE_SERVICE_NOT_PROMISED")]
    LanguageServiceNotPromised,
    #[serde(rename = "UPSTREAM_IMPLEMENTATION_ONLY")]
    UpstreamImplementationOnly,
    #[serde(rename = "UPSTREAM_HARNESS_ONLY")]
    UpstreamHarnessOnly,
    #[serde(rename = "RELEASE_INFRASTRUCTURE_ONLY")]
    ReleaseInfrastructureOnly,
}

/// The 4 required backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Backend {
    #[serde(rename = "check")]
    Check,
    #[serde(rename = "interpreter")]
    Interpreter,
    #[serde(rename = "jit")]
    Jit,
    #[serde(rename = "aot")]
    Aot,
}

/// The 4 timeout classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum TimeoutClass {
    #[serde(rename = "frontend")]
    Frontend,
    #[serde(rename = "execute")]
    Execute,
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "watch")]
    Watch,
}

/// Native difference classification for an expected baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum DifferenceClass {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "accepted")]
    Accepted,
    #[serde(rename = "triaged")]
    Triaged,
    #[serde(rename = "unclassified")]
    Unclassified,
}

/// Evidence confidence tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Confidence {
    #[serde(rename = "Verified")]
    Verified,
    #[serde(rename = "Probable")]
    Probable,
    #[serde(rename = "Speculative")]
    Speculative,
}

/// The top-level ledger document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TsLedger {
    pub schema_version: u32,
    pub oracle: Oracle,
    pub snapshot: Snapshot,
    pub entries: Vec<Entry>,
    pub totals: Totals,
}

impl TsLedger {
    /// Sort entries by `(input, facet, id)` and recompute snapshot counts
    /// and totals so the written bytes are deterministic and self-consistent.
    pub fn sort_and_recompute(&mut self) {
        self.entries.sort_by(|a, b| {
            (a.input.as_str(), a.facet.as_str(), a.id.as_str()).cmp(&(
                b.input.as_str(),
                b.facet.as_str(),
                b.id.as_str(),
            ))
        });

        self.snapshot.entry_count = self.entries.len();

        let distinct_inputs: BTreeSet<&str> =
            self.entries.iter().map(|e| e.input.as_str()).collect();
        self.snapshot.input_count = distinct_inputs.len();

        self.totals.entries = self.entries.len();
        self.totals.included = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Included)
            .count();
        self.totals.deferred = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Deferred)
            .count();
        self.totals.excluded = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Excluded)
            .count();
    }

    /// Validate the ledger in place before reading or writing.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != LEDGER_SCHEMA_VERSION {
            return Err(schema_error(format!(
                "schemaVersion must be {LEDGER_SCHEMA_VERSION}, found {}",
                self.schema_version
            )));
        }

        self.oracle.validate()?;

        if self.entries.is_empty() {
            return Err(schema_error("entries must contain at least one entry"));
        }

        let distinct_inputs: BTreeSet<&str> =
            self.entries.iter().map(|e| e.input.as_str()).collect();

        if self.snapshot.entry_count != self.entries.len() {
            return Err(schema_error(format!(
                "snapshot.entryCount ({}) must equal entries length ({})",
                self.snapshot.entry_count,
                self.entries.len()
            )));
        }

        if self.snapshot.input_count != distinct_inputs.len() {
            return Err(schema_error(format!(
                "snapshot.inputCount ({}) must equal distinct inputs ({})",
                self.snapshot.input_count,
                distinct_inputs.len()
            )));
        }

        self.snapshot.validate()?;

        for (index, entry) in self.entries.iter().enumerate() {
            entry.validate(index)?;
        }

        self.validate_sorted()?;
        self.validate_totals()?;

        Ok(())
    }

    /// Ensure entries are sorted by `(input, facet, id)`.
    fn validate_sorted(&self) -> Result<()> {
        let mut expected_order: Vec<_> = self.entries.iter().collect();
        expected_order.sort_by(|a, b| {
            (a.input.as_str(), a.facet.as_str(), a.id.as_str()).cmp(&(
                b.input.as_str(),
                b.facet.as_str(),
                b.id.as_str(),
            ))
        });

        for (actual, expected) in self.entries.iter().zip(expected_order.iter()) {
            if actual.id != expected.id
                || actual.input != expected.input
                || actual.facet != expected.facet
            {
                return Err(schema_error(format!(
                    "entries are not sorted by (input, facet, id); found ({}, {}, {}) before expected order",
                    actual.input,
                    actual.facet.as_str(),
                    actual.id
                )));
            }
        }

        Ok(())
    }

    fn validate_totals(&self) -> Result<()> {
        if self.totals.entries != self.entries.len() {
            return Err(schema_error(format!(
                "totals.entries ({}) must equal entries length ({})",
                self.totals.entries,
                self.entries.len()
            )));
        }

        let expected_included = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Included)
            .count();
        let expected_deferred = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Deferred)
            .count();
        let expected_excluded = self
            .entries
            .iter()
            .filter(|e| e.status == Status::Excluded)
            .count();

        if self.totals.included != expected_included {
            return Err(schema_error(format!(
                "totals.included ({}) must equal {}",
                self.totals.included, expected_included
            )));
        }
        if self.totals.deferred != expected_deferred {
            return Err(schema_error(format!(
                "totals.deferred ({}) must equal {}",
                self.totals.deferred, expected_deferred
            )));
        }
        if self.totals.excluded != expected_excluded {
            return Err(schema_error(format!(
                "totals.excluded ({}) must equal {}",
                self.totals.excluded, expected_excluded
            )));
        }

        if self.totals.discovered_inputs < self.snapshot.input_count {
            return Err(schema_error(format!(
                "totals.discoveredInputs ({}) must be at least snapshot.inputCount ({})",
                self.totals.discovered_inputs, self.snapshot.input_count
            )));
        }

        if self.totals.included + self.totals.deferred + self.totals.excluded != self.totals.entries
        {
            return Err(schema_error(format!(
                "totals do not sum: included ({}) + deferred ({}) + excluded ({}) != entries ({})",
                self.totals.included,
                self.totals.deferred,
                self.totals.excluded,
                self.totals.entries
            )));
        }

        Ok(())
    }
}

/// Oracle identity. Compatible with [`OraclePins`] without duplicating it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Oracle {
    pub npm: NpmOracle,
    pub compiler: CompilerOracle,
    pub suite: SuiteOracle,
}

impl Oracle {
    /// Project the ledger oracle onto the canonical [`OraclePins`] identity.
    pub fn to_oracle_pins(&self) -> OraclePins {
        OraclePins {
            npm_specifier: self.npm.specifier.clone(),
            npm_integrity: self.npm.integrity.clone(),
            compiler_repository: self.compiler.repository.clone(),
            compiler_commit: self.compiler.commit.clone(),
            compiler_tag: self.compiler.tag.clone(),
            suite_repository: self.suite.repository.clone(),
            suite_commit: self.suite.commit.clone(),
        }
    }

    fn validate(&self) -> Result<()> {
        let expected = OraclePins::expected();

        if self.npm.specifier != NPM_SPECIFIER {
            return Err(schema_error(format!(
                "oracle.npm.specifier must be `{NPM_SPECIFIER}`, found `{}`",
                self.npm.specifier
            )));
        }

        if !self.npm.integrity.starts_with("sha512-") {
            return Err(schema_error(format!(
                "oracle.npm.integrity must start with `sha512-`, found `{}`",
                self.npm.integrity
            )));
        }

        if self.npm.integrity != expected.npm_integrity {
            return Err(schema_error(
                "oracle.npm.integrity does not match the pinned TypeScript 7.0.2 integrity",
            ));
        }

        if !is_rfc3339_datetime(&self.npm.published_at) {
            return Err(schema_error(format!(
                "oracle.npm.publishedAt must be an RFC3339 date-time with a date, time, and offset, found `{}`",
                self.npm.published_at
            )));
        }

        if self.compiler.repository != expected.compiler_repository {
            return Err(schema_error(format!(
                "oracle.compiler.repository must be `{}`, found `{}`",
                expected.compiler_repository, self.compiler.repository
            )));
        }
        if self.compiler.commit != expected.compiler_commit {
            return Err(schema_error(format!(
                "oracle.compiler.commit must be `{}`, found `{}`",
                expected.compiler_commit, self.compiler.commit
            )));
        }
        if self.compiler.tag != expected.compiler_tag {
            return Err(schema_error(format!(
                "oracle.compiler.tag must be `{}`, found `{}`",
                expected.compiler_tag, self.compiler.tag
            )));
        }

        if self.suite.repository != expected.suite_repository {
            return Err(schema_error(format!(
                "oracle.suite.repository must be `{}`, found `{}`",
                expected.suite_repository, self.suite.repository
            )));
        }
        if self.suite.commit != expected.suite_commit {
            return Err(schema_error(format!(
                "oracle.suite.commit must be `{}`, found `{}`",
                expected.suite_commit, self.suite.commit
            )));
        }
        if self.suite.case_root != SUITE_CASE_ROOT {
            return Err(schema_error(format!(
                "oracle.suite.caseRoot must be `{SUITE_CASE_ROOT}`, found `{}`",
                self.suite.case_root
            )));
        }
        if self.suite.baseline_root != SUITE_BASELINE_ROOT {
            return Err(schema_error(format!(
                "oracle.suite.baselineRoot must be `{SUITE_BASELINE_ROOT}`, found `{}`",
                self.suite.baseline_root
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NpmOracle {
    pub specifier: String,
    pub integrity: String,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompilerOracle {
    pub repository: String,
    pub commit: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SuiteOracle {
    pub repository: String,
    pub commit: String,
    pub case_root: String,
    pub baseline_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Snapshot {
    pub digest: String,
    pub generated_at: String,
    pub entry_count: usize,
    pub input_count: usize,
}

impl Snapshot {
    fn validate(&self) -> Result<()> {
        if self.digest.is_empty() || !is_lowercase_hex(&self.digest, 64) {
            return Err(schema_error(format!(
                "snapshot.digest must be 64 lowercase hex chars, found `{}`",
                self.digest
            )));
        }

        if !is_rfc3339_datetime(&self.generated_at) {
            return Err(schema_error(format!(
                "snapshot.generatedAt must be an RFC3339 date-time with a date, time, and offset, found `{}`",
                self.generated_at
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Totals {
    pub discovered_inputs: usize,
    pub entries: usize,
    pub included: usize,
    pub deferred: usize,
    pub excluded: usize,
}

/// A single applicability cell.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub input: String,
    pub partition: Partition,
    pub facet: Facet,
    pub status: Status,
    pub surface: String,
    pub reason_code: ReasonCode,
    pub backends: Vec<Backend>,
    pub timeout_class: TimeoutClass,
    pub shard_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected: Vec<ExpectedRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    pub evidence: Vec<EvidenceRecord>,
}

impl Entry {
    fn validate(&self, index: usize) -> Result<()> {
        if self.id.is_empty() {
            return Err(entry_error(index, "id must not be empty"));
        }
        if self.input.is_empty() {
            return Err(entry_error(index, "input must not be empty"));
        }
        if self.surface.is_empty() {
            return Err(entry_error(index, "surface must not be empty"));
        }

        if self.backends.is_empty() {
            return Err(entry_error(
                index,
                "backends must contain at least one backend",
            ));
        }

        if self.evidence.is_empty() {
            return Err(entry_error(
                index,
                "evidence must contain at least one record",
            ));
        }

        if !is_lowercase_hex(&self.shard_key, 16) {
            return Err(entry_error(
                index,
                format!(
                    "shardKey must be 16 lowercase hex chars, found `{}`",
                    self.shard_key
                ),
            ));
        }

        for (record_index, record) in self.expected.iter().enumerate() {
            record.validate(index, record_index)?;
        }

        for (record_index, record) in self.evidence.iter().enumerate() {
            record.validate(index, record_index)?;
        }

        match self.status {
            Status::Included => {
                if !matches!(
                    self.reason_code,
                    ReasonCode::PromisedCompilerContract | ReasonCode::PromisedNodeApiContract
                ) {
                    return Err(entry_error(
                        index,
                        format!(
                            "included entries must have reasonCode PROMISED_COMPILER_CONTRACT or PROMISED_NODE_API_CONTRACT, found `{}`",
                            reason_code_str(self.reason_code)
                        ),
                    ));
                }
            }
            Status::Deferred => {
                if self.blocked_by.is_empty() {
                    return Err(entry_error(
                        index,
                        "deferred entries must have a non-empty blockedBy list",
                    ));
                }
            }
            Status::Excluded => {
                // Excluded rows keep the same evidence requirement as every other row.
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExpectedRecord {
    pub path: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_difference_class: Option<DifferenceClass>,
}

impl ExpectedRecord {
    fn validate(&self, entry_index: usize, record_index: usize) -> Result<()> {
        if self.path.is_empty() {
            return Err(entry_error(
                entry_index,
                format!("expected[{record_index}].path must not be empty"),
            ));
        }
        if !is_lowercase_hex(&self.sha256, 64) {
            return Err(entry_error(
                entry_index,
                format!(
                    "expected[{record_index}].sha256 must be 64 lowercase hex chars, found `{}`",
                    self.sha256
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub tier: u8,
    pub confidence: Confidence,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl EvidenceRecord {
    fn validate(&self, entry_index: usize, record_index: usize) -> Result<()> {
        if !(1..=5).contains(&self.tier) {
            return Err(entry_error(
                entry_index,
                format!(
                    "evidence[{record_index}].tier must be 1..=5, found {}",
                    self.tier
                ),
            ));
        }
        if !is_absolute_uri(&self.url) {
            return Err(entry_error(
                entry_index,
                format!(
                    "evidence[{record_index}].url must be an absolute URI with a valid scheme, found `{}`",
                    self.url
                ),
            ));
        }
        Ok(())
    }
}

/// Public reader. Parses a ledger from JSON and validates every rule.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsLedgerReader;

impl TsLedgerReader {
    /// Read a ledger from a JSON string.
    pub fn parse_str(text: &str) -> Result<TsLedger> {
        let ledger: TsLedger = serde_json::from_str(text)
            .map_err(|error| VerificationError::new(ErrorCode::Json, format!("{error}")))?;
        ledger.validate()?;
        Ok(ledger)
    }

    /// Read a ledger from a file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<TsLedger> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
        })?;
        Self::parse_str(&text)
    }

    /// Read a ledger from raw JSON bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<TsLedger> {
        let text = std::str::from_utf8(bytes).map_err(|error| {
            VerificationError::new(
                ErrorCode::Json,
                format!("ledger is not valid UTF-8: {error}"),
            )
        })?;
        Self::parse_str(text)
    }
}

/// Public writer. Emits stable, sorted JSON with LF endings and no trailing spaces.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsLedgerWriter;

impl TsLedgerWriter {
    /// Serialize a ledger to a stable, deterministic JSON string.
    pub fn to_string(ledger: &TsLedger) -> Result<String> {
        // Sort and recompute first, then validate the canonical form.
        let mut sorted = ledger.clone();
        sorted.sort_and_recompute();
        sorted.validate()?;

        let mut value = serde_json::to_value(&sorted)
            .map_err(|error| VerificationError::new(ErrorCode::Json, format!("{error}")))?;
        sort_json_keys(&mut value);

        let text = serde_json::to_string_pretty(&value)
            .map_err(|error| VerificationError::new(ErrorCode::Json, format!("{error}")))?;

        Ok(canonicalize_json(&text))
    }

    /// Serialize a ledger to a file.
    pub fn to_file(path: impl AsRef<Path>, ledger: &TsLedger) -> Result<()> {
        let path = path.as_ref();
        let bytes = Self::to_string(ledger)?;
        fs::write(path, bytes.as_bytes()).map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
        })
    }

    /// Serialize a ledger to stable JSON bytes.
    pub fn to_vec(ledger: &TsLedger) -> Result<Vec<u8>> {
        Self::to_string(ledger).map(|s| s.into_bytes())
    }
}

/// Validate an RFC3339 offset date-time, matching the schema `format: "date-time"`.
///
/// Reuses `toml::value::Datetime` parsing (the same parser the corpus manifests
/// use) and requires the date, time, and offset all to be present so local
/// date-times such as `2026-08-03T00:00:00` are rejected.
fn is_rfc3339_datetime(value: &str) -> bool {
    value
        .parse::<toml::value::Datetime>()
        .map(|datetime| {
            datetime.date.is_some() && datetime.time.is_some() && datetime.offset.is_some()
        })
        .unwrap_or(false)
}

/// Validate an absolute URI against the RFC 3986 grammar used by schema
/// `format: "uri"`.
fn is_absolute_uri(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once(':') else {
        return false;
    };
    let mut scheme_bytes = scheme.bytes();
    if !scheme_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        || !scheme_bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return false;
    }

    let (hierarchy_and_query, fragment) = split_once_optional(remainder, b'#');
    if fragment.is_some_and(|value| !valid_uri_bytes(value, is_query_or_fragment_char)) {
        return false;
    }
    let (hierarchy, query) = split_once_optional(hierarchy_and_query, b'?');
    if query.is_some_and(|value| !valid_uri_bytes(value, is_query_or_fragment_char)) {
        return false;
    }

    if let Some(authority_and_path) = hierarchy.strip_prefix("//") {
        let path_start = authority_and_path
            .find('/')
            .unwrap_or(authority_and_path.len());
        let (authority, path) = authority_and_path.split_at(path_start);
        return valid_authority(authority) && valid_uri_bytes(path, is_path_char);
    }
    valid_uri_bytes(hierarchy, is_path_char)
}

fn split_once_optional(value: &str, separator: u8) -> (&str, Option<&str>) {
    let Some(index) = value.bytes().position(|byte| byte == separator) else {
        return (value, None);
    };
    (&value[..index], Some(&value[index + 1..]))
}

fn valid_authority(authority: &str) -> bool {
    let (userinfo, host_and_port) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(userinfo, host)| (Some(userinfo), host));
    if userinfo.is_some_and(|value| !valid_uri_bytes(value, is_userinfo_char)) {
        return false;
    }

    if let Some(ip_literal) = host_and_port.strip_prefix('[') {
        let Some(close) = ip_literal.find(']') else {
            return false;
        };
        let (address, suffix) = ip_literal.split_at(close);
        return valid_ip_literal(address)
            && (suffix == "]"
                || suffix
                    .strip_prefix("]:")
                    .is_some_and(|port| port.bytes().all(|byte| byte.is_ascii_digit())));
    }
    if host_and_port.contains(['[', ']']) {
        return false;
    }

    let (host, port) = host_and_port
        .rsplit_once(':')
        .map_or((host_and_port, None), |(host, port)| (host, Some(port)));
    valid_uri_bytes(host, is_reg_name_char)
        && port.is_none_or(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_ip_literal(value: &str) -> bool {
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    let Some(version) = value.strip_prefix(['v', 'V']) else {
        return false;
    };
    let Some((hex, address)) = version.split_once('.') else {
        return false;
    };
    !hex.is_empty()
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !address.is_empty()
        && address
            .bytes()
            .all(|byte| is_unreserved(byte) || is_sub_delimiter(byte) || byte == b':')
}

fn valid_uri_bytes(value: &str, allowed: fn(u8) -> bool) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|escape| !escape.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !bytes[index].is_ascii() || !allowed(bytes[index]) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_path_char(byte: u8) -> bool {
    is_pchar(byte) || byte == b'/'
}

fn is_query_or_fragment_char(byte: u8) -> bool {
    is_pchar(byte) || matches!(byte, b'/' | b'?')
}

fn is_userinfo_char(byte: u8) -> bool {
    is_unreserved(byte) || is_sub_delimiter(byte) || byte == b':'
}

fn is_reg_name_char(byte: u8) -> bool {
    is_unreserved(byte) || is_sub_delimiter(byte)
}

fn is_pchar(byte: u8) -> bool {
    is_unreserved(byte) || is_sub_delimiter(byte) || matches!(byte, b':' | b'@')
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

/// Recursively sort every JSON object key lexicographically so the emitted bytes
/// never depend on struct field order or serde_json's map implementation.
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

fn canonicalize_json(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_end().replace('\r', ""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_lowercase_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn schema_error(detail: impl Into<String>) -> VerificationError {
    VerificationError::new(ErrorCode::Schema, detail)
}

fn entry_error(index: usize, detail: impl Into<String>) -> VerificationError {
    VerificationError::new(
        ErrorCode::Schema,
        format!("entry[{index}]: {}", detail.into()),
    )
}

fn reason_code_str(code: ReasonCode) -> &'static str {
    match code {
        ReasonCode::PromisedCompilerContract => "PROMISED_COMPILER_CONTRACT",
        ReasonCode::PromisedNodeApiContract => "PROMISED_NODE_API_CONTRACT",
        ReasonCode::PromisedNotImplemented => "PROMISED_NOT_IMPLEMENTED",
        ReasonCode::LanguageServiceNotPromised => "LANGUAGE_SERVICE_NOT_PROMISED",
        ReasonCode::UpstreamImplementationOnly => "UPSTREAM_IMPLEMENTATION_ONLY",
        ReasonCode::UpstreamHarnessOnly => "UPSTREAM_HARNESS_ONLY",
        ReasonCode::ReleaseInfrastructureOnly => "RELEASE_INFRASTRUCTURE_ONLY",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const VALID_SHARD_KEY: &str = "a1b2c3d4e5f60718";

    fn valid_oracle() -> Oracle {
        let pins = OraclePins::expected();
        Oracle {
            npm: NpmOracle {
                specifier: pins.npm_specifier,
                integrity: pins.npm_integrity,
                published_at: "2026-07-08T15:55:18.431Z".to_owned(),
            },
            compiler: CompilerOracle {
                repository: pins.compiler_repository,
                commit: pins.compiler_commit,
                tag: pins.compiler_tag,
            },
            suite: SuiteOracle {
                repository: pins.suite_repository,
                commit: pins.suite_commit,
                case_root: SUITE_CASE_ROOT.to_owned(),
                baseline_root: SUITE_BASELINE_ROOT.to_owned(),
            },
        }
    }

    fn valid_snapshot(entry_count: usize, input_count: usize) -> Snapshot {
        Snapshot {
            digest: VALID_DIGEST.to_owned(),
            generated_at: "2026-08-03T00:00:00Z".to_owned(),
            entry_count,
            input_count,
        }
    }

    fn valid_evidence() -> Vec<EvidenceRecord> {
        vec![EvidenceRecord {
            tier: 2,
            confidence: Confidence::Verified,
            url: "https://github.com/metaphorics/bamTiScript".to_owned(),
            note: None,
        }]
    }

    fn minimal_entry(
        id: &str,
        input: &str,
        facet: Facet,
        status: Status,
        reason_code: ReasonCode,
    ) -> Entry {
        Entry {
            id: id.to_owned(),
            input: input.to_owned(),
            partition: Partition::Compiler,
            facet,
            status,
            surface: "compiler.test".to_owned(),
            reason_code,
            backends: vec![Backend::Check],
            timeout_class: TimeoutClass::Frontend,
            shard_key: VALID_SHARD_KEY.to_owned(),
            expected: vec![ExpectedRecord {
                path: "tests/baselines/reference/test.txt".to_owned(),
                sha256: VALID_DIGEST.to_owned(),
                native_difference_class: Some(DifferenceClass::None),
            }],
            blocked_by: match status {
                Status::Deferred => vec!["slice:test".to_owned()],
                _ => Vec::new(),
            },
            evidence: valid_evidence(),
        }
    }

    fn valid_ledger() -> TsLedger {
        TsLedger {
            schema_version: LEDGER_SCHEMA_VERSION,
            oracle: valid_oracle(),
            snapshot: valid_snapshot(2, 1),
            entries: vec![
                minimal_entry(
                    "compiler/a.ts#diagnostics",
                    "tests/cases/compiler/a.ts",
                    Facet::Diagnostics,
                    Status::Included,
                    ReasonCode::PromisedCompilerContract,
                ),
                minimal_entry(
                    "compiler/a.ts#jsEmit",
                    "tests/cases/compiler/a.ts",
                    Facet::JsEmit,
                    Status::Deferred,
                    ReasonCode::PromisedNotImplemented,
                ),
            ],
            totals: Totals {
                discovered_inputs: 1,
                entries: 2,
                included: 1,
                deferred: 1,
                excluded: 0,
            },
        }
    }

    #[test]
    fn valid_schema_round_trip_through_reader_and_writer() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");
        let round = TsLedgerReader::parse_str(&json).expect("read");
        assert_eq!(round.schema_version, LEDGER_SCHEMA_VERSION);
        assert_eq!(round.oracle.to_oracle_pins(), OraclePins::expected());
        assert_eq!(round.entries.len(), 2);
        assert_eq!(round.totals.included, 1);
        assert_eq!(round.totals.deferred, 1);
    }

    #[test]
    fn unknown_field_at_root_is_rejected() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "rogue".to_owned(),
                serde_json::Value::String("value".to_owned()),
            );
        }
        let bad = serde_json::to_string(&value).unwrap();
        let err = TsLedgerReader::parse_str(&bad).expect_err("must reject root unknown field");
        assert_eq!(err.code(), ErrorCode::Json);
    }

    #[test]
    fn unknown_field_in_nested_entry_is_rejected() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let serde_json::Value::Object(map) = &mut value else {
            panic!("ledger root must be an object");
        };
        let Some(serde_json::Value::Array(entries)) = map.get_mut("entries") else {
            panic!("ledger entries must be an array");
        };
        let Some(serde_json::Value::Object(first)) = entries.first_mut() else {
            panic!("ledger entry must be an object");
        };
        first.insert("extra".to_owned(), serde_json::Value::Null);
        let bad = serde_json::to_string(&value).unwrap();
        let err = TsLedgerReader::parse_str(&bad).expect_err("must reject nested unknown field");
        assert_eq!(err.code(), ErrorCode::Json);
    }

    #[test]
    fn unknown_field_in_deep_nested_object_is_rejected() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let serde_json::Value::Object(map) = &mut value else {
            panic!("ledger root must be an object");
        };
        let Some(serde_json::Value::Object(oracle)) = map.get_mut("oracle") else {
            panic!("ledger oracle must be an object");
        };
        let Some(serde_json::Value::Object(npm)) = oracle.get_mut("npm") else {
            panic!("ledger npm oracle must be an object");
        };
        npm.insert("rogue".to_owned(), serde_json::Value::Null);
        let bad = serde_json::to_string(&value).unwrap();
        let err =
            TsLedgerReader::parse_str(&bad).expect_err("must reject oracle.npm unknown field");
        assert_eq!(err.code(), ErrorCode::Json);
    }

    #[test]
    fn unsorted_entries_are_rejected() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");

        // Parse, swap first two entries, re-emit so the bytes are unsorted.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let serde_json::Value::Object(map) = &mut value else {
            panic!("ledger root must be an object");
        };
        let Some(serde_json::Value::Array(entries)) = map.get_mut("entries") else {
            panic!("ledger entries must be an array");
        };
        entries.swap(0, 1);
        let bad = serde_json::to_string(&value).unwrap();

        let err = TsLedgerReader::parse_str(&bad).expect_err("must reject unsorted entries");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("not sorted"), "{err}");
    }

    #[test]
    fn deferred_without_blocked_by_is_rejected() {
        let mut ledger = valid_ledger();
        ledger.entries[1].blocked_by.clear();
        let err = TsLedgerWriter::to_string(&ledger).expect_err("write must fail validation");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("blockedBy"), "{err}");
    }

    #[test]
    fn included_with_deferred_or_excluded_reason_code_is_rejected() {
        let mut ledger = valid_ledger();
        ledger.entries[0].reason_code = ReasonCode::PromisedNotImplemented;
        let err = TsLedgerWriter::to_string(&ledger).expect_err("write must reject bad reason");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("reasonCode"), "{err}");
    }

    #[test]
    fn excluded_without_required_evidence_is_rejected() {
        let mut ledger = valid_ledger();
        ledger.entries[1].status = Status::Excluded;
        ledger.entries[1].reason_code = ReasonCode::LanguageServiceNotPromised;
        ledger.entries[1].blocked_by.clear();
        ledger.entries[1].evidence.clear();
        let err =
            TsLedgerWriter::to_string(&ledger).expect_err("write must reject missing evidence");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("evidence"), "{err}");
    }

    #[test]
    fn bad_shard_key_is_rejected() {
        let mut ledger = valid_ledger();
        ledger.entries[0].shard_key = "not-hex".to_owned();
        let err = TsLedgerWriter::to_string(&ledger).expect_err("write must reject bad shardKey");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("shardKey"), "{err}");
    }

    #[test]
    fn invalid_published_at_is_rejected() {
        // Date only: no time or offset.
        let mut ledger = valid_ledger();
        ledger.oracle.npm.published_at = "2026-07-08".to_owned();
        let err = TsLedgerWriter::to_string(&ledger)
            .expect_err("write must reject date-only publishedAt");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("publishedAt"), "{err}");

        // Local date-time: time present but no offset.
        let mut ledger = valid_ledger();
        ledger.oracle.npm.published_at = "2026-07-08T15:55:18".to_owned();
        let err = TsLedgerWriter::to_string(&ledger)
            .expect_err("write must reject offset-less publishedAt");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("publishedAt"), "{err}");
    }

    #[test]
    fn invalid_generated_at_is_rejected() {
        let mut ledger = valid_ledger();
        ledger.snapshot.generated_at = "2026-08-03T00:00:00".to_owned();
        let err = TsLedgerWriter::to_string(&ledger)
            .expect_err("write must reject offset-less generatedAt");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("generatedAt"), "{err}");
    }

    #[test]
    fn invalid_evidence_url_is_rejected() {
        // Relative reference: no scheme.
        let mut ledger = valid_ledger();
        ledger.entries[0].evidence[0].url = "tests/cases/compiler/a.ts".to_owned();
        let err =
            TsLedgerWriter::to_string(&ledger).expect_err("write must reject scheme-less url");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("url"), "{err}");

        // Whitespace inside the URI.
        let mut ledger = valid_ledger();
        ledger.entries[0].evidence[0].url = "ht tp://example.com".to_owned();
        let err = TsLedgerWriter::to_string(&ledger).expect_err("write must reject whitespace url");
        assert_eq!(err.code(), ErrorCode::Schema);
        assert!(err.to_string().contains("url"), "{err}");

        for malformed in ["https://example.com/%ZZ", "https://[", "https://host:bad"] {
            let mut ledger = valid_ledger();
            ledger.entries[0].evidence[0].url = malformed.to_owned();
            let error = TsLedgerWriter::to_string(&ledger)
                .expect_err("write must reject malformed absolute URI");
            assert_eq!(error.code(), ErrorCode::Schema);
            assert!(error.to_string().contains("url"), "{error}");
        }
    }

    #[test]
    fn valid_rfc3986_evidence_urls_are_accepted() {
        for url in [
            "https:",
            "urn:isbn:9780131103627",
            "file:///tmp/conformance.json",
            "https://user:pass@[2001:db8::1]:443/a%20b?first=?second#part?",
        ] {
            let mut ledger = valid_ledger();
            ledger.entries[0].evidence[0].url = url.to_owned();
            TsLedgerWriter::to_string(&ledger).expect("valid absolute URI");
        }
    }

    #[test]
    fn all_json_object_keys_are_lexicographically_sorted_in_written_bytes() {
        let ledger = valid_ledger();
        let json = TsLedgerWriter::to_string(&ledger).expect("write");
        assert_keys_lexicographically_sorted(&json);
    }

    /// Scan serde_json pretty output and assert every object's member keys are
    /// in lexicographic order in the actual emitted bytes. Array contexts only
    /// balance brackets; they never collect keys.
    fn assert_keys_lexicographically_sorted(text: &str) {
        let mut stack: Vec<(bool, Vec<String>)> = Vec::new();
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('"') {
                // `"key": …` member line.
                let key_end = rest.find('"').expect("member key closing quote");
                let tail = rest[key_end + 1..].trim();
                if let Some((is_object, keys)) = stack.last_mut()
                    && *is_object
                {
                    keys.push(rest[..key_end].to_owned());
                }
                if tail.ends_with('{') {
                    stack.push((true, Vec::new()));
                } else if tail.ends_with('[') {
                    stack.push((false, Vec::new()));
                }
                continue;
            }
            match line {
                "{" => stack.push((true, Vec::new())),
                "[" => stack.push((false, Vec::new())),
                "}" | "}," => {
                    let (is_object, keys) = stack.pop().expect("closing object");
                    if is_object {
                        let mut sorted = keys.clone();
                        sorted.sort();
                        assert_eq!(
                            keys, sorted,
                            "object keys must be lexicographically sorted in the written bytes"
                        );
                    }
                }
                "]" | "]," => {
                    let (is_object, _) = stack.pop().expect("closing array");
                    assert!(!is_object, "array close must close an array");
                }
                _ => {}
            }
        }
        assert!(stack.is_empty(), "unbalanced JSON scanner");
    }

    #[test]
    fn stable_sorted_bytes_across_insertion_order() {
        let a = minimal_entry(
            "compiler/a.ts#diagnostics",
            "tests/cases/compiler/a.ts",
            Facet::Diagnostics,
            Status::Included,
            ReasonCode::PromisedCompilerContract,
        );
        let b = minimal_entry(
            "compiler/b.ts#types",
            "tests/cases/compiler/b.ts",
            Facet::Types,
            Status::Included,
            ReasonCode::PromisedCompilerContract,
        );

        let mut ledger1 = valid_ledger();
        ledger1.entries = vec![a.clone(), b.clone()];
        ledger1.totals = Totals {
            discovered_inputs: 2,
            entries: 2,
            included: 2,
            deferred: 0,
            excluded: 0,
        };
        ledger1.snapshot = valid_snapshot(2, 2);

        let mut ledger2 = valid_ledger();
        ledger2.entries = vec![b.clone(), a.clone()];
        ledger2.totals = Totals {
            discovered_inputs: 2,
            entries: 2,
            included: 2,
            deferred: 0,
            excluded: 0,
        };
        ledger2.snapshot = valid_snapshot(2, 2);

        let bytes1 = TsLedgerWriter::to_vec(&ledger1).expect("write 1");
        let bytes2 = TsLedgerWriter::to_vec(&ledger2).expect("write 2");
        assert_eq!(bytes1, bytes2);

        // Also verify the writer put the sorted order into the bytes.
        let text = String::from_utf8(bytes1).unwrap();
        let pos_a = text.find("compiler/a.ts").unwrap();
        let pos_b = text.find("compiler/b.ts").unwrap();
        assert!(pos_a < pos_b, "entries must be sorted by input");
    }

    #[test]
    fn all_partitions_facets_reason_codes_backends_and_timeouts_are_represented() {
        let partitions = [
            Partition::Compiler,
            Partition::Conformance,
            Partition::Project,
            Partition::Projects,
            Partition::Transpile,
            Partition::UnitTests,
            Partition::Api,
            Partition::AstNav,
            Partition::Config,
            Partition::Fourslash,
            Partition::Lsp,
            Partition::TsBuild,
            Partition::TsBuildWatch,
            Partition::Tsc,
            Partition::TscWatch,
            Partition::TsOptions,
            Partition::Other,
        ];

        let facets = [
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

        let reasons = [
            (Status::Included, ReasonCode::PromisedCompilerContract),
            (Status::Included, ReasonCode::PromisedNodeApiContract),
            (Status::Deferred, ReasonCode::PromisedNotImplemented),
            (Status::Excluded, ReasonCode::LanguageServiceNotPromised),
            (Status::Excluded, ReasonCode::UpstreamImplementationOnly),
            (Status::Excluded, ReasonCode::UpstreamHarnessOnly),
            (Status::Excluded, ReasonCode::ReleaseInfrastructureOnly),
        ];

        let confidence = [
            Confidence::Verified,
            Confidence::Probable,
            Confidence::Speculative,
        ];

        let mut entries = Vec::with_capacity(partitions.len());
        for (index, partition) in partitions.iter().enumerate() {
            let facet = facets[index % facets.len()];
            let (status, reason_code) = reasons[index % reasons.len()];

            let backend = match index % 4 {
                0 => Backend::Check,
                1 => Backend::Interpreter,
                2 => Backend::Jit,
                _ => Backend::Aot,
            };

            let timeout_class = match index % 4 {
                0 => TimeoutClass::Frontend,
                1 => TimeoutClass::Execute,
                2 => TimeoutClass::Project,
                _ => TimeoutClass::Watch,
            };

            let difference_class = match index % 4 {
                0 => DifferenceClass::None,
                1 => DifferenceClass::Accepted,
                2 => DifferenceClass::Triaged,
                _ => DifferenceClass::Unclassified,
            };

            let partition_name = partition.as_str();
            let facet_name = facet.as_str();
            let input = format!("tests/cases/{partition_name}/{index}.ts");
            let id = format!("{partition_name}/{index}#{facet_name}");
            let surface = format!("{partition_name}.{facet_name}");

            entries.push(Entry {
                id,
                input,
                partition: *partition,
                facet,
                status,
                surface,
                reason_code,
                backends: vec![backend],
                timeout_class,
                shard_key: format!("{index:016x}"),
                expected: vec![ExpectedRecord {
                    path: format!("tests/baselines/reference/{partition_name}/{index}.txt"),
                    sha256: VALID_DIGEST.to_owned(),
                    native_difference_class: Some(difference_class),
                }],
                blocked_by: match status {
                    Status::Deferred => vec![format!("slice:{facet_name}")],
                    _ => Vec::new(),
                },
                evidence: vec![EvidenceRecord {
                    tier: ((index % 5) + 1) as u8,
                    confidence: confidence[index % confidence.len()],
                    url: format!("https://example.com/{partition_name}/{index}"),
                    note: if index % 2 == 0 {
                        Some("note".to_owned())
                    } else {
                        None
                    },
                }],
            });
        }

        let distinct_input_count = entries
            .iter()
            .map(|entry| entry.input.as_str())
            .collect::<BTreeSet<_>>()
            .len();

        let ledger = TsLedger {
            schema_version: LEDGER_SCHEMA_VERSION,
            oracle: valid_oracle(),
            snapshot: valid_snapshot(entries.len(), distinct_input_count),
            entries,
            totals: Totals {
                discovered_inputs: distinct_input_count,
                entries: 0,
                included: 0,
                deferred: 0,
                excluded: 0,
            },
        };

        let json = TsLedgerWriter::to_string(&ledger).expect("write full-coverage ledger");
        let round = TsLedgerReader::parse_str(&json).expect("read full-coverage ledger");

        assert_eq!(round.entries.len(), partitions.len());
        assert_eq!(round.snapshot.input_count, partitions.len());

        let mut seen_partitions = BTreeSet::new();
        let mut seen_facets = BTreeSet::new();
        let mut seen_reasons = BTreeSet::new();
        let mut seen_backends = BTreeSet::new();
        let mut seen_timeouts = BTreeSet::new();

        for entry in &round.entries {
            seen_partitions.insert(entry.partition);
            seen_facets.insert(entry.facet);
            seen_reasons.insert(entry.reason_code);
            seen_backends.extend(entry.backends.iter().copied());
            seen_timeouts.insert(entry.timeout_class);
        }

        assert_eq!(seen_partitions.len(), partitions.len());
        assert_eq!(seen_facets.len(), facets.len());
        assert_eq!(seen_reasons.len(), 7);
        assert_eq!(seen_backends.len(), 4);
        assert_eq!(seen_timeouts.len(), 4);
    }
}
