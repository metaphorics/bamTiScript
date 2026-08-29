//! Formal evidence → completeness-ledger projection for the P0.7 rows.
//!
//! A P0.7 formal row becomes `PASS` only when a dependency-closed G1–G6 audit
//! prefix is present and every required named proof receipt for that row binds
//! the exact [`ObligationKey`] and the current artifact digest. Status cannot
//! flip from a gate count, a stale digest, or a key that the receipt does not
//! name.
//!
//! Cluster integrator (Main) wires this file from `bamts-verification`:
//! `#[path = "../../../formal/ledger_wiring.rs"] pub mod ledger_wiring;`
//! Rebuild applies [`FormalLedgerRow`] onto generated ledger rows for the three
//! formal catalogs. This module does not write `proof/completeness-ledger.json`
//! or `verification/manifest.lock.json`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    ErrorCode, Gate, Result, VerificationError,
    evidence::{EvidenceRow, TerminalState, WorkingDirectoryPolicy},
    schema::{Catalog, VerificationManifest, sha256_hex},
    shard::{
        ExecutionMode, ObligationKey, digest_obligation_set, require_sha256, require_token,
        validate_catalog,
    },
};

/// Locked P0.7 inventory for the current TypeScript 7.0.2 completion wave.
pub const EXPECTED_P07_ROWS: usize = 264;
/// Locked `formal-lean` identifier count.
pub const FORMAL_LEAN_ROWS: usize = 80;
/// Locked `formal-quint` identifier count.
pub const FORMAL_QUINT_ROWS: usize = 74;
/// Locked `formal-redex` identifier count.
pub const FORMAL_REDEX_ROWS: usize = 110;

const _: () =
    assert!(FORMAL_LEAN_ROWS + FORMAL_QUINT_ROWS + FORMAL_REDEX_ROWS == EXPECTED_P07_ROWS);

/// Completeness owner recorded on every non-PASS P0.7 formal row.
pub const FORMAL_OWNER: &str = "P0.7";
/// Projection schema recorded on the generated row set.
pub const PROJECTION_SCHEMA: &str = "bamti.formal-ledger-projection/v1";
/// Closed formal catalogs owned by this projection.
pub const FORMAL_CATALOGS: [&str; 3] = ["formal-lean", "formal-quint", "formal-redex"];

const FORMAL_CONFIGURATION: &str = "default";
const FORMAL_PLATFORM: &str = "x86_64-unknown-linux-gnu";
const PROOF_OBSERVABLE: &str = "proof";
const BLOCKING_REASON_SUFFIX: &str = " has not passed its proof or property gate.";
const FORMAL_AUDIT_ARGV: [&str; 4] = ["formal", "audit", "--gates", "G1,G2,G5,G3,G4,G6"];

/// One locked formal catalog identifier with its obligation key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FormalObligation {
    catalog: String,
    case: String,
    key: ObligationKey,
}

impl FormalObligation {
    /// Catalog id (`formal-lean` / `formal-quint` / `formal-redex`).
    #[must_use]
    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    /// Catalog identifier (`file::name`, including `control::…` rows).
    #[must_use]
    pub fn case(&self) -> &str {
        &self.case
    }

    /// Canonical obligation key bound into receipts and evidence rows.
    #[must_use]
    pub fn key(&self) -> &ObligationKey {
        &self.key
    }

    /// Ledger row id `{catalog}:{case}`.
    #[must_use]
    pub fn ledger_id(&self) -> String {
        format!("{}:{}", self.catalog, self.case)
    }

    /// Completeness-ledger citation for this identifier.
    pub fn citation(&self) -> Result<String> {
        citation(self.catalog(), self.case())
    }

    /// Completeness-ledger matcher for this identifier.
    #[must_use]
    pub fn matcher(&self) -> String {
        matcher_for(self.case())
    }
}

/// Named G1–G6 proof receipt. The artifact path is the gate's canonical name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormalAuditReceipt {
    gate: Gate,
    checks: usize,
    artifact: String,
    evidence_digest: String,
    covered_keys: BTreeSet<ObligationKey>,
}

impl FormalAuditReceipt {
    /// Builds a receipt whose artifact name is the gate's named proof file.
    pub fn new(
        gate: Gate,
        checks: usize,
        evidence_digest: impl Into<String>,
        covered_keys: BTreeSet<ObligationKey>,
    ) -> Result<Self> {
        if checks == 0 {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("{} receipt cannot record zero checks", gate_label(gate)),
            ));
        }
        let artifact = named_proof_artifact(gate)?.to_owned();
        let evidence_digest = evidence_digest.into();
        require_sha256("evidence_digest", &evidence_digest)?;
        if covered_keys.is_empty() {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("{} receipt covers no obligation keys", gate_label(gate)),
            ));
        }
        let mut previous: Option<&ObligationKey> = None;
        for key in &covered_keys {
            key.validate()?;
            if let Some(prior) = previous
                && key <= prior
            {
                return Err(VerificationError::new(
                    ErrorCode::Schema,
                    format!(
                        "{} receipt keys are not strictly increasing around `{key}`",
                        gate_label(gate)
                    ),
                ));
            }
            previous = Some(key);
        }
        Ok(Self {
            gate,
            checks,
            artifact,
            evidence_digest,
            covered_keys,
        })
    }

    #[must_use]
    pub fn gate(&self) -> Gate {
        self.gate
    }

    #[must_use]
    pub fn checks(&self) -> usize {
        self.checks
    }

    #[must_use]
    pub fn artifact(&self) -> &str {
        &self.artifact
    }

    #[must_use]
    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    #[must_use]
    pub fn covered_keys(&self) -> &BTreeSet<ObligationKey> {
        &self.covered_keys
    }
}

/// One projected completeness-ledger row for a P0.7 formal obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormalLedgerRow {
    pub catalog: String,
    pub case: String,
    pub id: String,
    pub owner: String,
    pub citation: String,
    pub matcher: String,
    pub reason: String,
    pub state: TerminalState,
    pub key: ObligationKey,
    pub proof_digest: Option<String>,
}

/// Deterministic projection of G1–G6 receipts onto the formal universe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormalLedgerProjection {
    pub schema: &'static str,
    pub obligation_set_digest: String,
    pub pass_count: usize,
    pub rows: Vec<FormalLedgerRow>,
}

/// Canonical named proof artifact for a formal audit gate.
pub fn named_proof_artifact(gate: Gate) -> Result<&'static str> {
    match gate {
        Gate::G1 => Ok("formal/toolchains.toml"),
        Gate::G2 => Ok("verification/formal/quint/runs.json"),
        Gate::G3 => Ok("proof/lean-assumptions.json"),
        Gate::G4 => Ok("verification/manifest.lock.json"),
        Gate::G5 => Ok("formal/redex"),
        Gate::G6 => Ok("verification/formal/trace-fixtures.jsonl"),
        Gate::G0 => Err(VerificationError::new(
            ErrorCode::Usage,
            "G0 is not a formal proof receipt",
        )),
    }
}

/// Display label matching the formal audit CLI (`G1`…`G6`).
#[must_use]
pub fn gate_label(gate: Gate) -> &'static str {
    match gate {
        Gate::G0 => "G0",
        Gate::G1 => "G1",
        Gate::G2 => "G2",
        Gate::G3 => "G3",
        Gate::G4 => "G4",
        Gate::G5 => "G5",
        Gate::G6 => "G6",
    }
}

/// Gates that must name `catalog` before a row in that catalog can PASS.
pub fn required_gates(catalog: &str) -> Result<BTreeSet<Gate>> {
    let proving = proving_gate(catalog)?;
    Ok(BTreeSet::from([Gate::G1, proving, Gate::G4, Gate::G6]))
}

/// Catalog-specific proving gate: Quint G2, Lean G3, Redex G5.
pub fn proving_gate(catalog: &str) -> Result<Gate> {
    match catalog {
        "formal-quint" => Ok(Gate::G2),
        "formal-lean" => Ok(Gate::G3),
        "formal-redex" => Ok(Gate::G5),
        other => Err(unknown_formal_catalog(other)),
    }
}

/// Reads the three formal catalogs out of a verification manifest.
pub fn p07_universe(manifest: &VerificationManifest) -> Result<Vec<FormalObligation>> {
    let mut by_id: BTreeMap<&str, &Catalog> = BTreeMap::new();
    for catalog in &manifest.catalogs {
        if !is_formal_catalog(&catalog.id) {
            continue;
        }
        if by_id.insert(catalog.id.as_str(), catalog).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate formal catalog `{}`", catalog.id),
            ));
        }
    }
    let mut universe = Vec::new();
    for name in FORMAL_CATALOGS {
        let catalog = by_id.get(name).ok_or_else(|| {
            VerificationError::new(
                ErrorCode::SetMismatch,
                format!("verification manifest is missing formal catalog `{name}`"),
            )
        })?;
        if catalog.identifiers.is_empty() {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("formal catalog `{name}` has no identifiers"),
            ));
        }
        for case in &catalog.identifiers {
            universe.push(formal_obligation(name, case)?);
        }
    }
    let keys: Vec<ObligationKey> = universe.iter().map(|row| row.key.clone()).collect();
    validate_catalog(&keys)?;
    Ok(universe)
}

/// Rejects a universe that is not the locked 80/74/110 P0.7 inventory.
pub fn assert_locked_p07_inventory(universe: &[FormalObligation]) -> Result<()> {
    let mut lean = 0usize;
    let mut quint = 0usize;
    let mut redex = 0usize;
    for row in universe {
        match row.catalog() {
            "formal-lean" => lean += 1,
            "formal-quint" => quint += 1,
            "formal-redex" => redex += 1,
            other => return Err(unknown_formal_catalog(other)),
        }
    }
    if lean != FORMAL_LEAN_ROWS || quint != FORMAL_QUINT_ROWS || redex != FORMAL_REDEX_ROWS {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "formal P0.7 inventory is lean={lean} quint={quint} redex={redex}, expected {FORMAL_LEAN_ROWS}/{FORMAL_QUINT_ROWS}/{FORMAL_REDEX_ROWS}"
            ),
        ));
    }
    if universe.len() != EXPECTED_P07_ROWS {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "formal P0.7 universe has {} rows, expected {EXPECTED_P07_ROWS}",
                universe.len()
            ),
        ));
    }
    Ok(())
}

/// Projects current G1–G6 audit receipts onto `universe`.
///
/// Stale digests, unknown keys, wrong-catalog coverage, and a non-prefix gate
/// set fail closed. Missing coverage leaves the row `BLOCKING_FAIL`; it never
/// mints `PASS`.
pub fn project_formal_ledger(
    universe: &[FormalObligation],
    receipts: &[FormalAuditReceipt],
    current_digests: &BTreeMap<String, String>,
) -> Result<FormalLedgerProjection> {
    if universe.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "formal P0.7 universe must not be empty",
        ));
    }
    let keys: Vec<ObligationKey> = universe.iter().map(|row| row.key.clone()).collect();
    validate_catalog(&keys)?;
    let universe_keys: BTreeSet<ObligationKey> = keys.iter().cloned().collect();
    let by_gate = index_receipts(receipts, &universe_keys, current_digests)?;

    let mut rows = Vec::with_capacity(universe.len());
    let mut pass_count = 0usize;
    for obligation in universe {
        let required = required_gates(obligation.catalog())?;
        let pass = by_gate.len() == Gate::FORMAL_ORDER.len()
            && required.iter().all(|gate| {
                by_gate
                    .get(gate)
                    .is_some_and(|receipt| receipt.covered_keys.contains(obligation.key()))
            });
        let (state, owner, reason, proof_digest) = if pass {
            pass_count += 1;
            (
                TerminalState::Pass,
                String::new(),
                String::new(),
                Some(bound_proof_digest(&required, &by_gate)),
            )
        } else {
            (
                TerminalState::BlockingFail,
                FORMAL_OWNER.to_owned(),
                format!("{}{BLOCKING_REASON_SUFFIX}", obligation.case()),
                None,
            )
        };
        rows.push(FormalLedgerRow {
            catalog: obligation.catalog.clone(),
            case: obligation.case.clone(),
            id: obligation.ledger_id(),
            owner,
            citation: obligation.citation()?,
            matcher: obligation.matcher(),
            reason,
            state,
            key: obligation.key.clone(),
            proof_digest,
        });
    }

    Ok(FormalLedgerProjection {
        schema: PROJECTION_SCHEMA,
        obligation_set_digest: digest_obligation_set(keys.iter()),
        pass_count,
        rows,
    })
}

/// Evidence rows whose `proof` artifact is the bound receipt digest.
pub fn evidence_rows(projection: &FormalLedgerProjection) -> Result<Vec<EvidenceRow>> {
    let argv: Vec<String> = FORMAL_AUDIT_ARGV
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
    let observables = BTreeSet::from([PROOF_OBSERVABLE.to_owned()]);
    let mut rows = Vec::with_capacity(projection.rows.len());
    for row in &projection.rows {
        let (state, artifacts, detail) = if row.state.is_pass() {
            let digest = row.proof_digest.clone().ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::Schema,
                    format!("PASS row `{}` is missing its bound proof digest", row.id),
                )
            })?;
            let artifacts = BTreeMap::from([(PROOF_OBSERVABLE.to_owned(), digest)]);
            (TerminalState::Pass, artifacts, String::new())
        } else {
            (
                TerminalState::BlockingFail,
                BTreeMap::new(),
                row.reason.clone(),
            )
        };
        rows.push(EvidenceRow::new(
            row.key.clone(),
            argv.clone(),
            WorkingDirectoryPolicy::RepositoryRoot,
            observables.clone(),
            artifacts,
            state,
            0,
            detail,
        )?);
    }
    Ok(rows)
}

/// Catalog identifier additions or removals require a current G4 receipt that
/// covers the regenerated formal key set exactly.
pub fn validate_formal_extension(
    previous: &VerificationManifest,
    next: &VerificationManifest,
    receipts: &[FormalAuditReceipt],
    current_digests: &BTreeMap<String, String>,
) -> Result<()> {
    let previous_universe = p07_universe(previous)?;
    let next_universe = p07_universe(next)?;
    let previous_keys: BTreeSet<_> = previous_universe
        .iter()
        .map(|row| row.key.clone())
        .collect();
    let next_keys: BTreeSet<_> = next_universe.iter().map(|row| row.key.clone()).collect();
    if previous_keys == next_keys {
        return Ok(());
    }

    let g4 = receipts
        .iter()
        .find(|receipt| receipt.gate == Gate::G4)
        .ok_or_else(|| {
            VerificationError::new(
                ErrorCode::GateDependency,
                "formal catalog identifier changes require a current G4 authority receipt",
            )
        })?;
    if g4.artifact != named_proof_artifact(Gate::G4)? {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "G4 extension receipt is not the named manifest authority artifact",
        ));
    }
    let current = current_digests.get(&g4.artifact).ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Digest,
            format!("missing current digest for {}", g4.artifact),
        )
    })?;
    require_sha256("current G4 digest", current)?;
    if current != &g4.evidence_digest {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "G4 receipt digest {} is not the current artifact digest {current}",
                g4.evidence_digest
            ),
        ));
    }
    if g4.covered_keys != next_keys {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            "G4 receipt must cover the regenerated formal identifier set exactly",
        ));
    }
    Ok(())
}

/// SHA-256 of each named G1–G6 proof artifact under `root`.
pub fn current_named_digests(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut digests = BTreeMap::new();
    for gate in Gate::FORMAL_ORDER {
        let relative = named_proof_artifact(gate)?;
        digests.insert(relative.to_owned(), digest_named_artifact(root, gate)?);
    }
    Ok(digests)
}

/// Content digest of the named proof artifact for `gate`.
pub fn digest_named_artifact(root: &Path, gate: Gate) -> Result<String> {
    let relative = named_proof_artifact(gate)?;
    digest_path(&root.join(relative), relative)
}

fn digest_path(path: &Path, relative: &str) -> Result<String> {
    if path.is_file() {
        let bytes = fs::read(path).map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
        })?;
        return Ok(sha256_hex(&bytes));
    }
    if path.is_dir() {
        let mut files = Vec::new();
        collect_files(path, &mut files)?;
        files.sort();
        let mut hasher = Sha256::new();
        for file in files {
            let rel = file.strip_prefix(path).map_err(|error| {
                VerificationError::new(
                    ErrorCode::Io,
                    format!("cannot relativize {}: {error}", file.display()),
                )
            })?;
            let rel = rel.to_str().ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::Schema,
                    format!("{} is not UTF-8", file.display()),
                )
            })?;
            hasher.update(rel.as_bytes());
            hasher.update([0]);
            let bytes = fs::read(&file).map_err(|error| {
                VerificationError::new(ErrorCode::Io, format!("{}: {error}", file.display()))
            })?;
            hasher.update(&bytes);
            hasher.update([0x0a]);
        }
        return Ok(format!("{:x}", hasher.finalize()));
    }
    Err(VerificationError::new(
        ErrorCode::Io,
        format!(
            "named proof artifact `{relative}` is missing at {}",
            path.display()
        ),
    ))
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", dir.display()))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", dir.display()))
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!("{} contains a non-UTF-8 path", dir.display()),
            )
        })?;
        if name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            VerificationError::new(
                ErrorCode::Io,
                format!("{}: {error}", entry.path().display()),
            )
        })?;
        if file_type.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn index_receipts<'a>(
    receipts: &'a [FormalAuditReceipt],
    universe_keys: &BTreeSet<ObligationKey>,
    current_digests: &BTreeMap<String, String>,
) -> Result<BTreeMap<Gate, &'a FormalAuditReceipt>> {
    let mut by_gate = BTreeMap::new();
    for receipt in receipts {
        if by_gate.insert(receipt.gate, receipt).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate {} audit receipt", gate_label(receipt.gate)),
            ));
        }
        let expected = named_proof_artifact(receipt.gate)?;
        if receipt.artifact != expected {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "{} receipt artifact `{}` is not the named proof `{expected}`",
                    gate_label(receipt.gate),
                    receipt.artifact
                ),
            ));
        }
        let current = current_digests.get(&receipt.artifact).ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Digest,
                format!("missing current digest for {}", receipt.artifact),
            )
        })?;
        require_sha256("current artifact digest", current)?;
        if current != &receipt.evidence_digest {
            return Err(VerificationError::new(
                ErrorCode::Digest,
                format!(
                    "{} receipt digest {} is not the current digest {current}",
                    gate_label(receipt.gate),
                    receipt.evidence_digest
                ),
            ));
        }
        for key in &receipt.covered_keys {
            if !universe_keys.contains(key) {
                return Err(VerificationError::new(
                    ErrorCode::SetMismatch,
                    format!(
                        "{} receipt covers unknown obligation `{key}`",
                        gate_label(receipt.gate)
                    ),
                ));
            }
            let required = required_gates(key.catalog())?;
            if !required.contains(&receipt.gate) {
                return Err(VerificationError::new(
                    ErrorCode::SetMismatch,
                    format!(
                        "{} does not prove catalog `{}`",
                        gate_label(receipt.gate),
                        key.catalog()
                    ),
                ));
            }
        }
    }
    validate_gate_prefix(by_gate.keys().copied().collect())?;
    Ok(by_gate)
}

fn validate_gate_prefix(gates: BTreeSet<Gate>) -> Result<()> {
    if gates.is_empty() {
        return Ok(());
    }
    if gates.contains(&Gate::G0) {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            "G0 is not a formal proof receipt",
        ));
    }
    let mut highest = 0usize;
    for (index, gate) in Gate::FORMAL_ORDER.iter().enumerate() {
        if gates.contains(gate) {
            highest = index;
        }
    }
    let required: BTreeSet<Gate> = Gate::FORMAL_ORDER
        .iter()
        .take(highest + 1)
        .copied()
        .collect();
    if gates != required {
        return Err(VerificationError::new(
            ErrorCode::GateDependency,
            "formal audit receipts are not a dependency-closed prefix of Gate::FORMAL_ORDER",
        ));
    }
    Ok(())
}

fn bound_proof_digest(
    required: &BTreeSet<Gate>,
    by_gate: &BTreeMap<Gate, &FormalAuditReceipt>,
) -> String {
    let mut hasher = Sha256::new();
    for gate in Gate::FORMAL_ORDER {
        if !required.contains(&gate) {
            continue;
        }
        let receipt = by_gate
            .get(&gate)
            .expect("PASS rows already checked required receipts");
        hasher.update(gate_label(gate).as_bytes());
        hasher.update([0]);
        hasher.update(receipt.artifact.as_bytes());
        hasher.update([0]);
        hasher.update(receipt.evidence_digest.as_bytes());
        hasher.update([0x0a]);
    }
    format!("{:x}", hasher.finalize())
}

fn formal_obligation(catalog: &str, case: &str) -> Result<FormalObligation> {
    require_token("formal catalog", catalog)?;
    require_token("formal case", case)?;
    if !is_formal_catalog(catalog) {
        return Err(unknown_formal_catalog(catalog));
    }
    let key = ObligationKey::new(
        catalog,
        case,
        FORMAL_CONFIGURATION,
        ExecutionMode::Interpreter,
        FORMAL_PLATFORM,
    )?;
    Ok(FormalObligation {
        catalog: catalog.to_owned(),
        case: case.to_owned(),
        key,
    })
}

fn citation(catalog: &str, case: &str) -> Result<String> {
    let root = match catalog {
        "formal-quint" => "formal/quint",
        "formal-lean" => "formal/lean",
        "formal-redex" => "formal/redex",
        other => return Err(unknown_formal_catalog(other)),
    };
    let body = case.strip_prefix("control::").unwrap_or(case);
    Ok(format!("{root}/{}", body.replace("::", "#")))
}

fn matcher_for(case: &str) -> String {
    let mut matcher = String::from(r"\A");
    for character in case.chars() {
        if matches!(character, '.' | '-' | '~') {
            matcher.push('\\');
        }
        matcher.push(character);
    }
    matcher.push_str(r"\Z");
    matcher
}

fn is_formal_catalog(name: &str) -> bool {
    FORMAL_CATALOGS.contains(&name)
}

fn unknown_formal_catalog(name: &str) -> VerificationError {
    VerificationError::new(
        ErrorCode::Schema,
        format!("unknown formal catalog `{name}`"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::CatalogSource;

    #[test]
    fn locked_inventory_constants_match_p07_total() {
        assert_eq!(
            FORMAL_LEAN_ROWS + FORMAL_QUINT_ROWS + FORMAL_REDEX_ROWS,
            EXPECTED_P07_ROWS
        );
        assert_eq!(
            FORMAL_CATALOGS,
            ["formal-lean", "formal-quint", "formal-redex"]
        );
        assert_eq!(
            named_proof_artifact(Gate::G4).unwrap(),
            "verification/manifest.lock.json"
        );
        assert!(named_proof_artifact(Gate::G0).is_err());
    }

    #[test]
    fn citation_and_matcher_match_locked_ledger_shape() {
        let quint = formal_obligation(
            "formal-quint",
            "bytecode_cache_keys.qnt::CacheFieldIsolation",
        )
        .unwrap();
        assert_eq!(
            quint.citation().unwrap(),
            "formal/quint/bytecode_cache_keys.qnt#CacheFieldIsolation"
        );
        assert_eq!(
            quint.matcher(),
            r"\Abytecode_cache_keys\.qnt::CacheFieldIsolation\Z"
        );

        let lean =
            formal_obligation("formal-lean", "Bamti/Abi.lean::generation_never_wraps").unwrap();
        assert_eq!(
            lean.citation().unwrap(),
            "formal/lean/Bamti/Abi.lean#generation_never_wraps"
        );
        assert_eq!(
            lean.matcher(),
            r"\ABamti/Abi\.lean::generation_never_wraps\Z"
        );

        let relation =
            formal_obligation("formal-redex", "bytecode/compiler.rkt::compile-core").unwrap();
        assert_eq!(
            relation.citation().unwrap(),
            "formal/redex/bytecode/compiler.rkt#compile-core"
        );
        assert_eq!(
            relation.matcher(),
            r"\Abytecode/compiler\.rkt::compile\-core\Z"
        );

        let rule = formal_obligation(
            "formal-redex",
            "ecmascript/modules.rkt::rule::dynamic_import",
        )
        .unwrap();
        assert_eq!(
            rule.citation().unwrap(),
            "formal/redex/ecmascript/modules.rkt#rule#dynamic_import"
        );

        let control = formal_obligation(
            "formal-redex",
            "control::bytecode/simulation.rkt::src~bc::deterministic-examples",
        )
        .unwrap();
        assert_eq!(
            control.citation().unwrap(),
            "formal/redex/bytecode/simulation.rkt#src~bc#deterministic-examples"
        );
        assert_eq!(
            control.matcher(),
            r"\Acontrol::bytecode/simulation\.rkt::src\~bc::deterministic\-examples\Z"
        );
        assert_eq!(
            control.ledger_id(),
            "formal-redex:control::bytecode/simulation.rkt::src~bc::deterministic-examples"
        );
    }

    #[test]
    fn complete_named_receipts_flip_every_locked_row() {
        let universe = locked_universe();
        assert_locked_p07_inventory(&universe).unwrap();
        let (receipts, current) = complete_receipts(&universe);
        let projection = project_formal_ledger(&universe, &receipts, &current).unwrap();
        assert_eq!(projection.schema, PROJECTION_SCHEMA);
        assert_eq!(projection.rows.len(), EXPECTED_P07_ROWS);
        assert_eq!(projection.pass_count, EXPECTED_P07_ROWS);
        assert!(projection.rows.iter().all(|row| {
            row.state == TerminalState::Pass && row.owner.is_empty() && row.proof_digest.is_some()
        }));
        let evidence = evidence_rows(&projection).unwrap();
        assert_eq!(evidence.len(), EXPECTED_P07_ROWS);
        assert!(evidence.iter().all(|row| row.state().is_pass()));
        assert_eq!(
            projection.obligation_set_digest,
            digest_obligation_set(universe.iter().map(FormalObligation::key))
        );
    }

    #[test]
    fn missing_g6_receipt_leaves_every_row_blocking() {
        let universe = locked_universe();
        let (mut receipts, mut current) = complete_receipts(&universe);
        receipts.pop();
        current.remove(named_proof_artifact(Gate::G6).unwrap());
        let projection = project_formal_ledger(&universe, &receipts, &current).unwrap();
        assert_eq!(projection.pass_count, 0);
        assert!(projection.rows.iter().all(|row| {
            row.state == TerminalState::BlockingFail
                && row.owner == FORMAL_OWNER
                && row.proof_digest.is_none()
                && row.reason.ends_with(BLOCKING_REASON_SUFFIX)
        }));
    }

    #[test]
    fn stale_named_digest_fails_closed() {
        let universe = locked_universe();
        let (receipts, mut current) = complete_receipts(&universe);
        current.insert(
            named_proof_artifact(Gate::G2).unwrap().to_owned(),
            sha256_hex(b"mutated-g2"),
        );
        let error = project_formal_ledger(&universe, &receipts, &current).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Digest);
    }

    #[test]
    fn unknown_obligation_key_is_rejected() {
        let universe = locked_universe();
        let (mut receipts, current) = complete_receipts(&universe);
        let foreign = ObligationKey::new(
            "formal-quint",
            "missing.qnt::NotInUniverse",
            FORMAL_CONFIGURATION,
            ExecutionMode::Interpreter,
            FORMAL_PLATFORM,
        )
        .unwrap();
        let g2 = receipt_for(&receipts, Gate::G2);
        let mut covered = g2.covered_keys().clone();
        covered.insert(foreign);
        replace_receipt(
            &mut receipts,
            FormalAuditReceipt::new(Gate::G2, g2.checks(), g2.evidence_digest(), covered).unwrap(),
        );
        let error = project_formal_ledger(&universe, &receipts, &current).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SetMismatch);
    }

    #[test]
    fn proving_gate_cannot_cover_another_catalog() {
        let universe = locked_universe();
        let (mut receipts, current) = complete_receipts(&universe);
        let lean = universe
            .iter()
            .find(|row| row.catalog() == "formal-lean")
            .unwrap()
            .key()
            .clone();
        let g2 = receipt_for(&receipts, Gate::G2);
        let mut covered = g2.covered_keys().clone();
        covered.insert(lean);
        replace_receipt(
            &mut receipts,
            FormalAuditReceipt::new(Gate::G2, 1, g2.evidence_digest(), covered).unwrap(),
        );
        let error = project_formal_ledger(&universe, &receipts, &current).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SetMismatch);
    }

    #[test]
    fn g6_without_dependency_prefix_is_rejected() {
        let universe = locked_universe();
        let (receipts, current) = complete_receipts(&universe);
        let only_g6 = vec![receipt_for(&receipts, Gate::G6)];
        let error = project_formal_ledger(&universe, &only_g6, &current).unwrap_err();
        assert_eq!(error.code(), ErrorCode::GateDependency);
    }

    #[test]
    fn dropping_one_named_key_blocks_only_that_row() {
        let universe = locked_universe();
        let (mut receipts, current) = complete_receipts(&universe);
        let dropped = universe
            .iter()
            .find(|row| row.catalog() == "formal-quint")
            .unwrap()
            .key()
            .clone();
        let g2 = receipt_for(&receipts, Gate::G2);
        let mut covered = g2.covered_keys().clone();
        assert!(covered.remove(&dropped));
        replace_receipt(
            &mut receipts,
            FormalAuditReceipt::new(Gate::G2, g2.checks(), g2.evidence_digest(), covered).unwrap(),
        );
        let projection = project_formal_ledger(&universe, &receipts, &current).unwrap();
        assert_eq!(projection.pass_count, EXPECTED_P07_ROWS - 1);
        let dropped_row = projection
            .rows
            .iter()
            .find(|row| row.key == dropped)
            .unwrap();
        assert_eq!(dropped_row.state, TerminalState::BlockingFail);
        assert!(dropped_row.proof_digest.is_none());
        assert!(
            projection
                .rows
                .iter()
                .filter(|row| row.key != dropped)
                .all(|row| row.state.is_pass())
        );
    }

    #[test]
    fn bound_proof_digest_changes_when_a_required_receipt_digest_changes() {
        let universe = locked_universe();
        let (receipts, current) = complete_receipts(&universe);
        let first = project_formal_ledger(&universe, &receipts, &current).unwrap();
        let mut mutated = receipts;
        let mut mutated_current = current;
        let digest = sha256_hex(b"g4-mutated");
        let g4 = receipt_for(&mutated, Gate::G4);
        replace_receipt(
            &mut mutated,
            FormalAuditReceipt::new(
                Gate::G4,
                g4.checks(),
                digest.clone(),
                g4.covered_keys().clone(),
            )
            .unwrap(),
        );
        mutated_current.insert(named_proof_artifact(Gate::G4).unwrap().to_owned(), digest);
        let second = project_formal_ledger(&universe, &mutated, &mutated_current).unwrap();
        assert_eq!(second.pass_count, EXPECTED_P07_ROWS);
        assert_ne!(
            first.rows[0].proof_digest.as_deref(),
            second.rows[0].proof_digest.as_deref()
        );
    }

    #[test]
    fn duplicate_gate_receipt_is_rejected() {
        let universe = locked_universe();
        let (receipts, current) = complete_receipts(&universe);
        let mut duplicated = receipts.clone();
        duplicated.push(receipts[0].clone());
        let error = project_formal_ledger(&universe, &duplicated, &current).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Duplicate);
    }

    #[test]
    fn extension_without_g4_is_rejected() {
        let previous = locked_manifest();
        let mut next = previous.clone();
        push_quint_identifier(&mut next, "model.qnt::prop999");
        let error = validate_formal_extension(&previous, &next, &[], &BTreeMap::new()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::GateDependency);
    }

    #[test]
    fn extension_with_exact_g4_coverage_is_accepted() {
        let previous = locked_manifest();
        let mut next = previous.clone();
        push_quint_identifier(&mut next, "model.qnt::prop999");
        let next_universe = p07_universe(&next).unwrap();
        assert_eq!(next_universe.len(), EXPECTED_P07_ROWS + 1);
        let keys: BTreeSet<_> = next_universe.iter().map(|row| row.key.clone()).collect();
        let digest = sha256_hex(b"g4-extended");
        let g4 = FormalAuditReceipt::new(Gate::G4, 1, digest.clone(), keys).unwrap();
        let current = BTreeMap::from([(g4.artifact().to_owned(), digest)]);
        validate_formal_extension(&previous, &next, &[g4], &current).unwrap();
    }

    #[test]
    fn named_directory_digest_is_path_order_stable() {
        let root = scratch("dir-digest");
        let redex = root.join("formal/redex");
        fs::create_dir_all(redex.join("nested")).unwrap();
        fs::write(redex.join("b.rkt"), b"second").unwrap();
        fs::write(redex.join("nested/a.rkt"), b"first").unwrap();
        let digest = digest_path(&redex, "formal/redex").unwrap();
        fs::write(redex.join("nested/a.rkt"), b"mutated").unwrap();
        let mutated = digest_path(&redex, "formal/redex").unwrap();
        assert_ne!(digest, mutated);
        let _ = fs::remove_dir_all(&root);
    }

    fn receipt_for(receipts: &[FormalAuditReceipt], gate: Gate) -> FormalAuditReceipt {
        receipts
            .iter()
            .find(|receipt| receipt.gate() == gate)
            .cloned()
            .expect("receipt for gate")
    }

    fn replace_receipt(receipts: &mut [FormalAuditReceipt], receipt: FormalAuditReceipt) {
        let gate = receipt.gate();
        let slot = receipts
            .iter_mut()
            .find(|candidate| candidate.gate() == gate)
            .expect("existing receipt for gate");
        *slot = receipt;
    }

    fn push_quint_identifier(manifest: &mut VerificationManifest, identifier: &str) {
        let catalog = manifest
            .catalogs
            .iter_mut()
            .find(|catalog| catalog.id == "formal-quint")
            .expect("formal-quint catalog");
        catalog.identifiers.push(identifier.to_owned());
        catalog.identifier_count = catalog.identifiers.len();
        catalog.identifiers_sha256 = crate::schema::identifiers_sha256(&catalog.identifiers);
    }

    fn locked_universe() -> Vec<FormalObligation> {
        p07_universe(&locked_manifest()).unwrap()
    }

    fn locked_manifest() -> VerificationManifest {
        VerificationManifest {
            schema: "bamti.verification-manifest/v1".to_owned(),
            source_ledger_sha256: "a".repeat(64),
            catalogs: vec![
                catalog("formal-lean", FORMAL_LEAN_ROWS, "Bamti/Test.lean"),
                catalog("formal-quint", FORMAL_QUINT_ROWS, "model.qnt"),
                catalog("formal-redex", FORMAL_REDEX_ROWS, "lang.rkt"),
            ],
        }
    }

    fn catalog(id: &str, count: usize, file: &str) -> Catalog {
        let identifiers: Vec<String> = (0..count)
            .map(|index| format!("{file}::prop{index:03}"))
            .collect();
        Catalog {
            extractor: serde_json::json!({"kind": "test"}),
            id: id.to_owned(),
            identifier_count: count,
            identifiers_sha256: crate::schema::identifiers_sha256(&identifiers),
            identifiers,
            source: CatalogSource {
                pin: "test".to_owned(),
                url: "local://test".to_owned(),
                digest_algorithm: "sha256".to_owned(),
                digest: "b".repeat(64),
            },
        }
    }

    fn complete_receipts(
        universe: &[FormalObligation],
    ) -> (Vec<FormalAuditReceipt>, BTreeMap<String, String>) {
        let mut receipts = Vec::new();
        let mut current = BTreeMap::new();
        for gate in Gate::FORMAL_ORDER {
            let covered = universe
                .iter()
                .filter(|row| required_gates(row.catalog()).unwrap().contains(&gate))
                .map(|row| row.key.clone())
                .collect();
            let digest = sha256_hex(gate_label(gate).as_bytes());
            let receipt = FormalAuditReceipt::new(gate, 1, digest.clone(), covered).unwrap();
            current.insert(receipt.artifact().to_owned(), digest);
            receipts.push(receipt);
        }
        (receipts, current)
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bamts-f11-{name}-{}-{}",
            std::process::id(),
            name.len()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
