//! Strict serde/TOML cell-classification policy records.
//!
//! Non-PASS completeness states are a closed set. Each record names exact cell
//! IDs or a matcher whose expanded sorted cell set and digest are stored and
//! revalidated against the supplied catalog universe. Workers never choose a
//! classification: [`validate_classifications`] returns a deterministic exact
//! map covering every universe cell.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    ErrorCode, Result, VerificationError,
    catalog::{CatalogCell, PublicSurface, RunnerKind},
    schema::CLASSIFICATION_DIR,
};

/// Schema version accepted by this validator.
pub const SCHEMA_VERSION: &str = "bamti.cell-classification/v1";

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Closed non-PASS completeness states. `PASS` is never a policy state; cells
/// without a matching record are mapped to [`ClassificationState::Pass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NonPassState {
    BlockingFail,
    InapplicableLanguageService,
    InapplicableOutOfScopeHostFeature,
    InapplicableV8Internal,
    InapplicableCatalogError,
    ExternalBlocked,
}

impl NonPassState {
    /// Catalog-error rows never count as complete, even when recorded.
    pub const fn is_completion_eligible(self) -> bool {
        match self {
            Self::InapplicableLanguageService
            | Self::InapplicableOutOfScopeHostFeature
            | Self::InapplicableV8Internal
            | Self::ExternalBlocked => true,
            Self::BlockingFail | Self::InapplicableCatalogError => false,
        }
    }

    const fn excludes_public_operations(self) -> bool {
        matches!(
            self,
            Self::InapplicableLanguageService
                | Self::InapplicableOutOfScopeHostFeature
                | Self::InapplicableV8Internal
                | Self::InapplicableCatalogError
        )
    }
}

/// How a policy attests the public API surface of its matched cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicSurfaceDisposition {
    /// Public operations stay on the blocking completeness path.
    Blocking,
    /// Matched cells are attested to sit outside the public API surface.
    NonPublic,
}

/// Exact per-cell classification. Workers look this up; they do not choose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClassificationState {
    Pass,
    NonPass(NonPassState),
}

impl ClassificationState {
    pub const fn is_completion_eligible(self) -> bool {
        match self {
            Self::Pass => true,
            Self::NonPass(state) => state.is_completion_eligible(),
        }
    }
}

/// Validated policy document. Selectors are already resolved to exact cell IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationDocument {
    pub schema: String,
    pub policies: Vec<ClassificationPolicy>,
}

/// One non-PASS policy after selector expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationPolicy {
    pub id: String,
    pub state: NonPassState,
    pub cells: Vec<String>,
    pub citation: String,
    pub reason: String,
    pub evidence_sha256: String,
    pub owner: String,
    pub public_surface: PublicSurfaceDisposition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    schema: String,
    #[serde(default, rename = "policy")]
    policies: Vec<RawPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    id: String,
    state: NonPassState,
    #[serde(default)]
    cells: Option<Vec<String>>,
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    expanded_cells: Option<Vec<String>>,
    #[serde(default)]
    expanded_digest: Option<String>,
    citation: String,
    reason: String,
    evidence_sha256: String,
    owner: String,
    public_surface: PublicSurfaceDisposition,
}

/// Parses TOML policy records, validates them against a sorted `CatalogCell`
/// universe, and returns the exact cell-id → state mapping.
pub fn validate_classifications(
    toml: &str,
    universe: &[CatalogCell],
) -> Result<BTreeMap<String, ClassificationState>> {
    let parsed = parse_classification_toml(toml)?;
    apply_classifications(&parsed, universe)
}

/// Loads one optional `<catalog>.toml` record per catalog and returns one exact
/// classification for every supplied logical cell.
pub fn load_classifications(
    root: &Path,
    universes: &BTreeMap<String, Vec<CatalogCell>>,
) -> Result<BTreeMap<String, ClassificationState>> {
    let directory = root.join(CLASSIFICATION_DIR);
    let expected_files: BTreeSet<String> = universes
        .keys()
        .map(|catalog| format!("{catalog}.toml"))
        .collect();
    if directory.exists() {
        if !directory.is_dir() {
            return Err(VerificationError::new(
                ErrorCode::Io,
                format!("{} is not a directory", directory.display()),
            ));
        }
        for entry in fs::read_dir(&directory).map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("{}: {error}", directory.display()))
        })? {
            let entry = entry.map_err(|error| {
                VerificationError::new(ErrorCode::Io, format!("{}: {error}", directory.display()))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                VerificationError::new(
                    ErrorCode::Io,
                    format!("{}: {error}", entry.path().display()),
                )
            })?;
            let name = entry.file_name().into_string().map_err(|_| {
                VerificationError::new(
                    ErrorCode::Schema,
                    format!("{} contains a non-UTF-8 policy name", directory.display()),
                )
            })?;
            if !file_type.is_file() || !expected_files.contains(&name) {
                return Err(VerificationError::new(
                    ErrorCode::SetMismatch,
                    format!(
                        "{} contains unexpected policy `{name}`",
                        directory.display()
                    ),
                ));
            }
        }
    }

    let mut classifications = BTreeMap::new();
    for (catalog, universe) in universes {
        let path = directory.join(format!("{catalog}.toml"));
        let catalog_classifications = if path.exists() {
            let source = fs::read_to_string(&path).map_err(|error| {
                VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
            })?;
            validate_classifications(&source, universe)?
        } else {
            universe
                .iter()
                .map(|cell| (cell.rendered_identity(), ClassificationState::Pass))
                .collect()
        };
        for (id, state) in catalog_classifications {
            if classifications.insert(id.clone(), state).is_some() {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("classification universe contains duplicate `{id}`"),
                ));
            }
        }
    }
    Ok(classifications)
}

fn parse_classification_toml(toml: &str) -> Result<RawParsedDocument> {
    let raw: RawDocument = toml::from_str(toml).map_err(|error| {
        VerificationError::new(ErrorCode::Toml, format!("classification policy: {error}"))
    })?;
    if raw.schema != SCHEMA_VERSION {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("expected schema `{SCHEMA_VERSION}`, found `{}`", raw.schema),
        ));
    }
    Ok(RawParsedDocument {
        policies: raw.policies,
    })
}

struct RawParsedDocument {
    policies: Vec<RawPolicy>,
}

fn apply_classifications(
    parsed: &RawParsedDocument,
    universe: &[CatalogCell],
) -> Result<BTreeMap<String, ClassificationState>> {
    let index = universe_index(universe)?;
    let mut assigned: BTreeMap<String, (String, NonPassState)> = BTreeMap::new();
    let mut seen_policy_ids = BTreeSet::new();

    for raw in &parsed.policies {
        let policy = validate_policy(raw, universe, &index)?;
        if !seen_policy_ids.insert(policy.id.clone()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate policy `{}`", policy.id),
            ));
        }
        for cell_id in &policy.cells {
            if let Some((owner, _)) =
                assigned.insert(cell_id.clone(), (policy.id.clone(), policy.state))
            {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!(
                        "duplicate policy for cell `{cell_id}` (`{owner}` and `{}`)",
                        policy.id
                    ),
                ));
            }
        }
    }

    let mut mapping = BTreeMap::new();
    for cell in universe {
        let identity = cell.rendered_identity();
        let state = match assigned.get(&identity) {
            Some((_, state)) => ClassificationState::NonPass(*state),
            None => ClassificationState::Pass,
        };
        mapping.insert(identity, state);
    }
    Ok(mapping)
}

fn universe_index(universe: &[CatalogCell]) -> Result<BTreeMap<String, usize>> {
    let mut index = BTreeMap::new();
    let mut previous: Option<String> = None;
    for (offset, cell) in universe.iter().enumerate() {
        require_nonempty("catalog cell authority", &cell.authority)?;
        require_nonempty("catalog cell case", &cell.case)?;
        require_nonempty("catalog cell configuration", &cell.configuration)?;
        let identity = cell.rendered_identity();
        require_nonempty("catalog cell id", &identity)?;
        if let Some(previous_id) = previous.as_deref() {
            if identity == previous_id {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("duplicate catalog cell `{identity}`"),
                ));
            }
            if identity.as_str() < previous_id {
                return Err(VerificationError::new(
                    ErrorCode::Schema,
                    "catalog cell universe is not strictly sorted by id",
                ));
            }
        }
        index.insert(identity.clone(), offset);
        previous = Some(identity);
    }
    Ok(index)
}

fn validate_policy(
    raw: &RawPolicy,
    universe: &[CatalogCell],
    index: &BTreeMap<String, usize>,
) -> Result<ClassificationPolicy> {
    require_nonempty("policy id", &raw.id)?;
    require_nonempty("citation", &raw.citation)?;
    require_nonempty("reason", &raw.reason)?;
    require_nonempty("owner", &raw.owner)?;
    validate_evidence(&raw.evidence_sha256)?;
    if matches!(raw.state, NonPassState::InapplicableCatalogError) {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            format!(
                "policy `{}`: INAPPLICABLE_CATALOG_ERROR is never completion-eligible",
                raw.id
            ),
        ));
    }

    let cells = resolve_selector(raw, universe, index)?;
    if cells.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!("policy `{}` matches no catalog cells", raw.id),
        ));
    }

    for cell_id in &cells {
        let cell = &universe[index[cell_id]];
        reject_public_api_exclusion(raw, cell)?;
        reject_false_non_public_attestation(raw, cell)?;
    }

    if raw.state == NonPassState::InapplicableLanguageService && selector_is_directory_wide(raw) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "policy `{}`: directory-wide language-service exclusions are forbidden",
                raw.id
            ),
        ));
    }

    if raw.state == NonPassState::InapplicableLanguageService && raw.matcher.is_some() {
        reject_blanket_language_service_set(raw, universe, index, &cells)?;
    }

    Ok(ClassificationPolicy {
        id: raw.id.clone(),
        state: raw.state,
        cells,
        citation: raw.citation.clone(),
        reason: raw.reason.clone(),
        evidence_sha256: raw.evidence_sha256.clone(),
        owner: raw.owner.clone(),
        public_surface: raw.public_surface,
    })
}

fn resolve_selector(
    raw: &RawPolicy,
    universe: &[CatalogCell],
    index: &BTreeMap<String, usize>,
) -> Result<Vec<String>> {
    match (raw.cells.as_ref(), raw.matcher.as_ref()) {
        (Some(cells), None) => {
            if raw.expanded_cells.is_some() || raw.expanded_digest.is_some() {
                return Err(VerificationError::new(
                    ErrorCode::Schema,
                    format!(
                        "policy `{}`: exact cell IDs cannot declare matcher expansion fields",
                        raw.id
                    ),
                ));
            }
            resolve_exact_cells(raw, cells, index)
        }
        (None, Some(pattern)) => resolve_matcher(raw, pattern, universe, index),
        (Some(_), Some(_)) => Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "policy `{}` cannot declare both exact cell IDs and a matcher",
                raw.id
            ),
        )),
        (None, None) => Err(VerificationError::new(
            ErrorCode::Schema,
            format!("policy `{}` must declare `cells` or `matcher`", raw.id),
        )),
    }
}

fn resolve_exact_cells(
    raw: &RawPolicy,
    cells: &[String],
    index: &BTreeMap<String, usize>,
) -> Result<Vec<String>> {
    if cells.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!("policy `{}` matches no catalog cells", raw.id),
        ));
    }
    let mut resolved = Vec::with_capacity(cells.len());
    let mut seen = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for cell_id in cells {
        require_nonempty("cell id", cell_id)?;
        if !seen.insert(cell_id.as_str()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("policy `{}` repeats cell `{cell_id}`", raw.id),
            ));
        }
        if let Some(previous_id) = previous
            && cell_id.as_str() < previous_id
        {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("policy `{}` cell IDs are not strictly ordered", raw.id),
            ));
        }
        if !index.contains_key(cell_id) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("policy `{}` names missing cell `{cell_id}`", raw.id),
            ));
        }
        resolved.push(cell_id.clone());
        previous = Some(cell_id.as_str());
    }
    Ok(resolved)
}

fn resolve_matcher(
    raw: &RawPolicy,
    pattern: &str,
    universe: &[CatalogCell],
    index: &BTreeMap<String, usize>,
) -> Result<Vec<String>> {
    require_nonempty("matcher", pattern)?;
    let stored_cells = raw.expanded_cells.as_ref().ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!("policy `{}` matcher must store `expanded_cells`", raw.id),
        )
    })?;
    let stored_digest = raw.expanded_digest.as_deref().ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!("policy `{}` matcher must store `expanded_digest`", raw.id),
        )
    })?;
    validate_stored_expansion(raw, stored_cells, stored_digest, index)?;

    let live = expand_matcher(pattern, universe);
    if live.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "policy `{}` matcher `{pattern}` matches no catalog cells",
                raw.id
            ),
        ));
    }

    let stored_set: BTreeSet<&str> = stored_cells.iter().map(String::as_str).collect();
    let live_set: BTreeSet<&str> = live.iter().map(String::as_str).collect();
    if live_set.difference(&stored_set).next().is_some() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "policy `{}` matcher overmatches the stored cell set",
                raw.id
            ),
        ));
    }
    if stored_set.difference(&live_set).next().is_some() || live != *stored_cells {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "policy `{}`: catalog drift invalidates matcher expansion",
                raw.id
            ),
        ));
    }

    let live_digest = cell_ids_digest(&live);
    if live_digest != stored_digest {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "policy `{}`: matcher digest does not match expanded cells",
                raw.id
            ),
        ));
    }

    Ok(live)
}

fn validate_stored_expansion(
    raw: &RawPolicy,
    stored_cells: &[String],
    stored_digest: &str,
    index: &BTreeMap<String, usize>,
) -> Result<()> {
    if stored_cells.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!("policy `{}` matcher stores an empty expansion", raw.id),
        ));
    }
    let mut previous: Option<&str> = None;
    let mut seen = BTreeSet::new();
    for cell_id in stored_cells {
        require_nonempty("expanded cell id", cell_id)?;
        if !seen.insert(cell_id.as_str()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("policy `{}` expansion repeats cell `{cell_id}`", raw.id),
            ));
        }
        if let Some(previous_id) = previous
            && cell_id.as_str() < previous_id
        {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "policy `{}` expanded_cells are not strictly ordered",
                    raw.id
                ),
            ));
        }
        if !index.contains_key(cell_id) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "policy `{}`: catalog drift, stored cell `{cell_id}` is missing",
                    raw.id
                ),
            ));
        }
        previous = Some(cell_id.as_str());
    }
    if !is_sha256(stored_digest) {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!("policy `{}` has a malformed expanded_digest", raw.id),
        ));
    }
    let expected = cell_ids_digest(stored_cells);
    if expected != stored_digest {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "policy `{}`: stored matcher digest does not match expanded_cells",
                raw.id
            ),
        ));
    }
    Ok(())
}

fn expand_matcher(pattern: &str, universe: &[CatalogCell]) -> Vec<String> {
    universe
        .iter()
        .filter(|cell| cell_matches(pattern, cell))
        .map(CatalogCell::rendered_identity)
        .collect()
}

fn cell_matches(pattern: &str, cell: &CatalogCell) -> bool {
    matches_pattern(pattern, &cell.rendered_identity()) || matches_pattern(pattern, &cell.case)
}

fn matches_pattern(pattern: &str, value: &str) -> bool {
    if value == pattern {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return value == prefix || value.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let needle = format!("{prefix}/");
        if let Some(rest) = value.strip_prefix(&needle) {
            return !rest.is_empty() && !rest.contains('/');
        }
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix('/') {
        return value.starts_with(&format!("{prefix}/"));
    }
    false
}

fn selector_is_directory_wide(raw: &RawPolicy) -> bool {
    raw.matcher
        .as_deref()
        .is_some_and(pattern_is_directory_wide)
}

fn pattern_is_directory_wide(pattern: &str) -> bool {
    pattern.ends_with('/')
        || pattern.ends_with("/*")
        || pattern.ends_with("/**")
        || pattern.contains("/**")
}

fn reject_blanket_language_service_set(
    raw: &RawPolicy,
    universe: &[CatalogCell],
    index: &BTreeMap<String, usize>,
    cells: &[String],
) -> Result<()> {
    let matched_ls: Vec<&CatalogCell> = cells
        .iter()
        .map(|id| &universe[index[id]])
        .filter(|cell| is_language_service_cell(cell))
        .collect();
    if matched_ls.len() < 2 {
        return Ok(());
    }
    let Some(directory) = common_language_service_directory(&matched_ls) else {
        return Ok(());
    };
    let universe_ls_in_directory = universe
        .iter()
        .filter(|cell| is_language_service_cell(cell))
        .filter(|cell| identifier_in_directory(&cell.case, directory))
        .count();
    if matched_ls.len() == universe_ls_in_directory {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "policy `{}`: directory-wide language-service exclusions are forbidden",
                raw.id
            ),
        ));
    }
    Ok(())
}

fn common_language_service_directory<'a>(cells: &'a [&CatalogCell]) -> Option<&'a str> {
    let first = cells.first()?.case.as_str();
    let slash = first.rfind('/')?;
    let directory = &first[..slash];
    cells
        .iter()
        .all(|cell| identifier_in_directory(&cell.case, directory))
        .then_some(directory)
}

fn identifier_in_directory(identifier: &str, directory: &str) -> bool {
    identifier
        .strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn reject_public_api_exclusion(raw: &RawPolicy, cell: &CatalogCell) -> Result<()> {
    if is_public_api_reachable(cell) && raw.state.excludes_public_operations() {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            format!(
                "policy `{}`: public API cell `{}` remains blocking",
                raw.id,
                cell.rendered_identity()
            ),
        ));
    }
    Ok(())
}

fn reject_false_non_public_attestation(raw: &RawPolicy, cell: &CatalogCell) -> Result<()> {
    if raw.public_surface == PublicSurfaceDisposition::NonPublic && is_public_api_reachable(cell) {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            format!(
                "policy `{}`: public API cell `{}` cannot be attested non_public",
                raw.id,
                cell.rendered_identity()
            ),
        ));
    }
    Ok(())
}

fn is_language_service_cell(cell: &CatalogCell) -> bool {
    cell.runner == RunnerKind::Fourslash
}

fn is_public_api_reachable(cell: &CatalogCell) -> bool {
    !matches!(
        cell.public_surface,
        PublicSurface::InternalHarness | PublicSurface::ProposalStage
    )
}

fn validate_evidence(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            "non-PASS policy has empty evidence_sha256",
        ));
    }
    if !is_sha256(value) {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            "non-PASS policy evidence_sha256 must be lowercase SHA-256 hex",
        ));
    }
    if value == EMPTY_SHA256 {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            "non-PASS policy has empty evidence",
        ));
    }
    Ok(())
}

fn require_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Transition,
            format!("non-PASS policy has empty `{field}`"),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn cell_ids_digest<'a, I>(ids: I) -> String
where
    I: IntoIterator<Item = &'a String>,
{
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ObservableKind;

    const LS_COMPLETION: &str = "tests/cases/fourslash/completion.ts";
    const LS_RENAME: &str = "tests/cases/fourslash/rename.ts";
    const COMPILER_ARRAY: &str = "tests/cases/compiler/arrayLiteral1.ts";
    const PUBLIC_API: &str = "tests/cases/compiler/APISample_compile.ts";
    const V8_INTERNAL: &str = "src/node_v8.cc";
    const HOST_FEATURE: &str = "test/parallel/test-dgram-multicast.js";

    fn sha(input: &str) -> String {
        format!("{:x}", Sha256::digest(input.as_bytes()))
    }

    fn evidence() -> String {
        sha("classification-evidence")
    }

    fn cell(identifier: &str, language_service: bool, public_api: bool) -> CatalogCell {
        let authority = if identifier.starts_with("src/") || identifier.starts_with("test/") {
            "node-24.18.0"
        } else {
            "typescript-7.0.2"
        };
        CatalogCell {
            authority: authority.to_owned(),
            runner: if language_service {
                RunnerKind::Fourslash
            } else {
                RunnerKind::Compiler
            },
            case: identifier.to_owned(),
            configuration: "default".to_owned(),
            observable: if language_service {
                ObservableKind::Types
            } else {
                ObservableKind::Diagnostics
            },
            public_surface: if public_api {
                PublicSurface::CompilerApi
            } else {
                PublicSurface::InternalHarness
            },
        }
    }

    fn universe() -> Vec<CatalogCell> {
        let mut cells = vec![
            cell(HOST_FEATURE, false, false),
            cell(V8_INTERNAL, false, false),
            cell(PUBLIC_API, false, true),
            cell(COMPILER_ARRAY, false, false),
            cell(LS_COMPLETION, true, false),
            cell(LS_RENAME, true, false),
        ];
        cells.sort();
        cells
    }

    fn cell_id(identifier: &str) -> String {
        universe()
            .into_iter()
            .find(|cell| cell.case == identifier)
            .expect("cell")
            .rendered_identity()
    }

    fn header() -> String {
        format!("schema = \"{SCHEMA_VERSION}\"\n")
    }

    fn exact_policy(id: &str, state: &str, cells: &[&str], surface: &str) -> String {
        let mut body = header();
        body.push_str("[[policy]]\n");
        body.push_str(&format!("id = \"{id}\"\n"));
        body.push_str(&format!("state = \"{state}\"\n"));
        body.push_str("cells = [");
        body.push_str(
            &cells
                .iter()
                .map(|cell| format!("\"{}\"", cell_id(cell)))
                .collect::<Vec<_>>()
                .join(", "),
        );
        body.push_str("]\n");
        body.push_str("citation = \"TypeScript src/harness/fourslashRunner.ts\"\n");
        body.push_str("reason = \"language-service protocol coverage, not emit\"\n");
        body.push_str(&format!("evidence_sha256 = \"{}\"\n", evidence()));
        body.push_str("owner = \"completeness\"\n");
        body.push_str(&format!("public_surface = \"{surface}\"\n"));
        body
    }

    fn matcher_policy(
        id: &str,
        state: &str,
        pattern: &str,
        expanded: &[&str],
        surface: &str,
    ) -> String {
        let ids: Vec<String> = expanded.iter().map(|cell| cell_id(cell)).collect();
        let digest = cell_ids_digest(&ids);
        let mut body = header();
        body.push_str("[[policy]]\n");
        body.push_str(&format!("id = \"{id}\"\n"));
        body.push_str(&format!("state = \"{state}\"\n"));
        body.push_str(&format!("matcher = \"{pattern}\"\n"));
        body.push_str("expanded_cells = [");
        body.push_str(
            &ids.iter()
                .map(|id| format!("\"{id}\""))
                .collect::<Vec<_>>()
                .join(", "),
        );
        body.push_str("]\n");
        body.push_str(&format!("expanded_digest = \"{digest}\"\n"));
        body.push_str("citation = \"Node src/node_v8.cc internals\"\n");
        body.push_str("reason = \"V8 embedder internals are out of emit scope\"\n");
        body.push_str(&format!("evidence_sha256 = \"{}\"\n", evidence()));
        body.push_str("owner = \"completeness\"\n");
        body.push_str(&format!("public_surface = \"{surface}\"\n"));
        body
    }

    fn classify_ok(toml: &str) -> BTreeMap<String, ClassificationState> {
        validate_classifications(toml, &universe()).expect("policy should validate")
    }

    #[test]
    fn non_pass_requires_exact_evidence() {
        let valid = exact_policy(
            "ls-completion",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[LS_COMPLETION],
            "non_public",
        );
        assert!(validate_classifications(&valid, &universe()).is_ok());

        let missing_evidence =
            valid.replace(&format!("evidence_sha256 = \"{}\"\n", evidence()), "");
        assert_eq!(
            validate_classifications(&missing_evidence, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Toml
        );

        let empty_evidence = valid.replace(
            &format!("evidence_sha256 = \"{}\"", evidence()),
            "evidence_sha256 = \"\"",
        );
        assert_eq!(
            validate_classifications(&empty_evidence, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let empty_hash = valid.replace(&evidence(), EMPTY_SHA256);
        assert_eq!(
            validate_classifications(&empty_hash, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let empty_citation = valid.replace(
            "citation = \"TypeScript src/harness/fourslashRunner.ts\"",
            "citation = \"\"",
        );
        assert_eq!(
            validate_classifications(&empty_citation, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let empty_reason = valid.replace(
            "reason = \"language-service protocol coverage, not emit\"",
            "reason = \"   \"",
        );
        assert_eq!(
            validate_classifications(&empty_reason, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let empty_owner = valid.replace("owner = \"completeness\"", "owner = \"\"");
        assert_eq!(
            validate_classifications(&empty_owner, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let malformed = valid.replace(&evidence(), "not-a-digest");
        assert_eq!(
            validate_classifications(&malformed, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Digest
        );
    }

    #[test]
    fn catalog_error_is_never_complete() {
        assert!(!NonPassState::InapplicableCatalogError.is_completion_eligible());
        assert!(
            !ClassificationState::NonPass(NonPassState::InapplicableCatalogError)
                .is_completion_eligible()
        );

        let toml = exact_policy(
            "catalog-bug",
            "INAPPLICABLE_CATALOG_ERROR",
            &[COMPILER_ARRAY],
            "non_public",
        );
        let error = validate_classifications(&toml, &universe()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Transition);
        assert!(error.to_string().contains("never completion-eligible"));
    }

    #[test]
    fn rejects_blanket_language_service_exclusion() {
        let glob = matcher_policy(
            "ls-dir",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            "tests/cases/fourslash/**",
            &[LS_COMPLETION, LS_RENAME],
            "non_public",
        );
        let error = validate_classifications(&glob, &universe()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Schema);
        assert!(
            error
                .to_string()
                .contains("directory-wide language-service")
        );

        let star = matcher_policy(
            "ls-star",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            "tests/cases/fourslash/*",
            &[LS_COMPLETION, LS_RENAME],
            "non_public",
        );
        assert_eq!(
            validate_classifications(&star, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );

        let exact = exact_policy(
            "ls-one",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[LS_COMPLETION],
            "non_public",
        );
        let map = classify_ok(&exact);
        assert_eq!(
            map.get(&cell_id(LS_COMPLETION)),
            Some(&ClassificationState::NonPass(
                NonPassState::InapplicableLanguageService
            ))
        );
        assert_eq!(
            map.get(&cell_id(LS_RENAME)),
            Some(&ClassificationState::Pass)
        );
    }

    #[test]
    fn public_api_remains_blocking() {
        let toml = exact_policy(
            "drop-public",
            "INAPPLICABLE_OUT_OF_SCOPE_HOST_FEATURE",
            &[PUBLIC_API],
            "non_public",
        );
        let error = validate_classifications(&toml, &universe()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Transition);
        assert!(error.to_string().contains("remains blocking"));

        let blocking_fail = exact_policy("public-fail", "BLOCKING_FAIL", &[PUBLIC_API], "blocking");
        let map = classify_ok(&blocking_fail);
        assert_eq!(
            map.get(&cell_id(PUBLIC_API)),
            Some(&ClassificationState::NonPass(NonPassState::BlockingFail))
        );
        assert!(!NonPassState::BlockingFail.is_completion_eligible());
    }

    #[test]
    fn matcher_drift_invalidates_policy() {
        let stale_digest = matcher_policy(
            "v8",
            "INAPPLICABLE_V8_INTERNAL",
            "src/node_v8.cc",
            &[V8_INTERNAL],
            "non_public",
        )
        .replace(&cell_ids_digest(&[cell_id(V8_INTERNAL)]), &sha("stale"));
        assert_eq!(
            validate_classifications(&stale_digest, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Digest
        );

        let mut drifted = universe();
        drifted.push(cell("src/node_v8_extras.cc", false, false));
        drifted.sort();
        let stored = matcher_policy(
            "v8-src",
            "INAPPLICABLE_V8_INTERNAL",
            "src/**",
            &[V8_INTERNAL],
            "non_public",
        );
        let error = validate_classifications(&stored, &drifted).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SetMismatch);
        assert!(error.to_string().contains("overmatch") || error.to_string().contains("drift"));
    }

    #[test]
    fn rejects_duplicate_policy() {
        let one = exact_policy(
            "ls-one",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[LS_COMPLETION],
            "non_public",
        );
        let duplicate_id = format!(
            "{one}{}",
            exact_policy(
                "ls-one",
                "INAPPLICABLE_LANGUAGE_SERVICE",
                &[LS_RENAME],
                "non_public",
            )
            .replacen(&header(), "", 1)
        );
        assert_eq!(
            validate_classifications(&duplicate_id, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Duplicate
        );

        let overlapping = format!(
            "{one}{}",
            exact_policy(
                "ls-two",
                "INAPPLICABLE_LANGUAGE_SERVICE",
                &[LS_COMPLETION],
                "non_public",
            )
            .replacen(&header(), "", 1)
        );
        assert_eq!(
            validate_classifications(&overlapping, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Duplicate
        );
    }

    #[test]
    fn rejects_missing_cell() {
        let mut toml = header();
        toml.push_str("[[policy]]\n");
        toml.push_str("id = \"ghost\"\n");
        toml.push_str("state = \"INAPPLICABLE_V8_INTERNAL\"\n");
        toml.push_str("cells = [\"node-24.18.0:src/does-not-exist.cc\"]\n");
        toml.push_str("citation = \"missing cell\"\n");
        toml.push_str("reason = \"names a cell the catalog does not contain\"\n");
        toml.push_str(&format!("evidence_sha256 = \"{}\"\n", evidence()));
        toml.push_str("owner = \"completeness\"\n");
        toml.push_str("public_surface = \"non_public\"\n");
        let error = validate_classifications(&toml, &universe()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SetMismatch);
        assert!(error.to_string().contains("missing cell"));
    }

    #[test]
    fn rejects_changed_cell_universe() {
        let policy = matcher_policy(
            "v8",
            "INAPPLICABLE_V8_INTERNAL",
            "src/node_v8.cc",
            &[V8_INTERNAL],
            "non_public",
        );
        assert!(validate_classifications(&policy, &universe()).is_ok());

        let mut removed = universe();
        removed.retain(|cell| cell.case != V8_INTERNAL);
        let error = validate_classifications(&policy, &removed).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SetMismatch);
        assert!(
            error.to_string().contains("catalog drift") || error.to_string().contains("missing")
        );

        let mut resorted = universe();
        resorted.reverse();
        assert_eq!(
            validate_classifications(&policy, &resorted)
                .unwrap_err()
                .code(),
            ErrorCode::Schema
        );
    }

    #[test]
    fn rejects_unknown_state() {
        let toml = exact_policy(
            "skip",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[LS_COMPLETION],
            "non_public",
        )
        .replace("INAPPLICABLE_LANGUAGE_SERVICE", "WAIVED");
        assert_eq!(
            validate_classifications(&toml, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Toml
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let toml = format!(
            "{}extra = true\n",
            exact_policy(
                "ls-one",
                "INAPPLICABLE_LANGUAGE_SERVICE",
                &[LS_COMPLETION],
                "non_public",
            )
        );
        assert_eq!(
            validate_classifications(&toml, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Toml
        );

        let policy_field = exact_policy(
            "ls-one",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[LS_COMPLETION],
            "non_public",
        ) + "note = \"worker choice\"\n";
        assert_eq!(
            validate_classifications(&policy_field, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Toml
        );
    }

    #[test]
    fn rejects_api_reachable_public_operations() {
        let language_service = exact_policy(
            "api-as-ls",
            "INAPPLICABLE_LANGUAGE_SERVICE",
            &[PUBLIC_API],
            "blocking",
        );
        assert_eq!(
            validate_classifications(&language_service, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );

        let host = exact_policy(
            "api-as-host",
            "INAPPLICABLE_OUT_OF_SCOPE_HOST_FEATURE",
            &[PUBLIC_API],
            "blocking",
        );
        let error = validate_classifications(&host, &universe()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Transition);
        assert!(error.to_string().contains("remains blocking"));

        let v8 = exact_policy(
            "api-as-v8",
            "INAPPLICABLE_V8_INTERNAL",
            &[PUBLIC_API],
            "blocking",
        );
        assert_eq!(
            validate_classifications(&v8, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::Transition
        );
    }

    #[test]
    fn matcher_and_exact_policies_are_deterministic() {
        let mut toml = matcher_policy(
            "v8",
            "INAPPLICABLE_V8_INTERNAL",
            "src/node_v8.cc",
            &[V8_INTERNAL],
            "non_public",
        );
        toml.push_str(
            &exact_policy(
                "ls-one",
                "INAPPLICABLE_LANGUAGE_SERVICE",
                &[LS_COMPLETION],
                "non_public",
            )
            .replacen(&header(), "", 1),
        );
        toml.push_str(
            &exact_policy(
                "host",
                "INAPPLICABLE_OUT_OF_SCOPE_HOST_FEATURE",
                &[HOST_FEATURE],
                "non_public",
            )
            .replacen(&header(), "", 1),
        );

        let first = classify_ok(&toml);
        let second = classify_ok(&toml);
        assert_eq!(first, second);
        assert_eq!(first.len(), universe().len());
        assert_eq!(
            first.get(&cell_id(V8_INTERNAL)),
            Some(&ClassificationState::NonPass(
                NonPassState::InapplicableV8Internal
            ))
        );
        assert_eq!(
            first.get(&cell_id(COMPILER_ARRAY)),
            Some(&ClassificationState::Pass)
        );
        assert_eq!(
            first.get(&cell_id(PUBLIC_API)),
            Some(&ClassificationState::Pass)
        );
    }

    #[test]
    fn unmatched_matcher_is_rejected() {
        let ids = [cell_id(V8_INTERNAL)];
        let digest = cell_ids_digest(&ids);
        let mut toml = header();
        toml.push_str("[[policy]]\n");
        toml.push_str("id = \"unmatched\"\n");
        toml.push_str("state = \"INAPPLICABLE_V8_INTERNAL\"\n");
        toml.push_str("matcher = \"src/missing.cc\"\n");
        toml.push_str(&format!("expanded_cells = [\"{}\"]\n", ids[0]));
        toml.push_str(&format!("expanded_digest = \"{digest}\"\n"));
        toml.push_str("citation = \"stale matcher\"\n");
        toml.push_str("reason = \"pattern matches nothing in the universe\"\n");
        toml.push_str(&format!("evidence_sha256 = \"{}\"\n", evidence()));
        toml.push_str("owner = \"completeness\"\n");
        toml.push_str("public_surface = \"non_public\"\n");
        assert_eq!(
            validate_classifications(&toml, &universe())
                .unwrap_err()
                .code(),
            ErrorCode::SetMismatch
        );
    }
}
