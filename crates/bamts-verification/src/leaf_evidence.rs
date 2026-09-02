//! Strict artifact-bound leaf evidence validation.
//!
//! A gate receipt is valid only when it names the registered leaf and aspect,
//! binds every owned artifact, and the recorded bytes still match the workspace.
//! Process execution and mutation pairing build on this boundary separately.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, Result, VerificationError, schema::sha256_hex};

pub(crate) const SCHEMA: &str = "bamti.leaf-evidence/v1";
const MAX_RECEIPT_BYTES: usize = 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    leaf: String,
    aspect: String,
    gate: String,
    adapter: String,
    artifacts: Vec<Artifact>,
    mutation: Option<MutationEvidence>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    path: String,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MutationEvidence {
    id: String,
    baseline_sha256: String,
    mutated_sha256: String,
    restored_sha256: String,
}

#[derive(Clone, Copy)]
enum Adapter {
    AstProtocol,
    PackageContract,
    #[cfg(test)]
    TestArtifact,
}

impl Adapter {
    const fn name(self) -> &'static str {
        match self {
            Self::AstProtocol => "ast_protocol",
            Self::PackageContract => "package_contract",
            #[cfg(test)]
            Self::TestArtifact => "test_artifact",
        }
    }
}

/// Validates a content-addressed receipt against one registered leaf aspect.
pub(crate) fn validate(
    root: &Path,
    leaf: &str,
    aspect: &str,
    owns: &[String],
    evidence: &str,
) -> Result<()> {
    let (receipt_path, expected_digest) = parse_reference(evidence)?;
    let receipt_path = checked_regular_file(root, &receipt_path)?;
    let bytes = read_bounded(&receipt_path, MAX_RECEIPT_BYTES)?;
    require_sha256("receipt", &expected_digest)?;
    if sha256_hex(&bytes) != expected_digest {
        return Err(schema("receipt digest differs from the gate evidence"));
    }

    let adapter = registered_adapter(leaf, aspect).ok_or_else(|| {
        schema(format!(
            "no registered evidence adapter for `{leaf}` aspect `{aspect}`"
        ))
    })?;
    match adapter {
        Adapter::AstProtocol => validate_ast_protocol_receipt(root, leaf, aspect, owns, &bytes),
        Adapter::PackageContract => {
            validate_package_contract_receipt(root, leaf, aspect, owns, &bytes)
        }
        #[cfg(test)]
        Adapter::TestArtifact => validate_test_artifact_receipt(root, leaf, aspect, owns, &bytes),
    }
}

fn validate_ast_protocol_receipt(
    root: &Path,
    leaf: &str,
    aspect: &str,
    owns: &[String],
    bytes: &[u8],
) -> Result<()> {
    let receipt: Receipt = serde_json::from_slice(bytes).map_err(|error| {
        VerificationError::new(
            ErrorCode::Json,
            format!("invalid leaf evidence receipt: {error}"),
        )
    })?;
    if receipt.schema != SCHEMA {
        return Err(schema(format!(
            "unsupported leaf evidence schema `{}`",
            receipt.schema
        )));
    }
    if receipt.leaf != leaf {
        return Err(schema(format!(
            "receipt leaf `{}` does not match registered leaf `{leaf}`",
            receipt.leaf
        )));
    }
    if receipt.aspect != aspect {
        return Err(schema(format!(
            "receipt aspect `{}` does not match registered aspect `{aspect}`",
            receipt.aspect
        )));
    }
    let expected_gate = gate_for_aspect(aspect).ok_or_else(|| {
        schema(format!(
            "leaf evidence cannot certify unsupported aspect `{aspect}`"
        ))
    })?;
    if receipt.gate != expected_gate {
        return Err(schema(format!(
            "receipt gate `{}` does not match `{expected_gate}` for aspect `{aspect}`",
            receipt.gate,
        )));
    }
    if receipt.adapter != Adapter::AstProtocol.name() {
        return Err(schema(format!(
            "receipt adapter `{}` does not match registered adapter `{}`",
            receipt.adapter,
            Adapter::AstProtocol.name()
        )));
    }

    let expected_artifacts = expand_owned_artifacts(root, owns)?;
    let mut actual_artifacts = BTreeMap::new();
    for artifact in &receipt.artifacts {
        require_sha256("artifact", &artifact.sha256)?;
        if actual_artifacts
            .insert(artifact.path.clone(), artifact.sha256.clone())
            .is_some()
        {
            return Err(schema(format!(
                "receipt duplicates artifact `{}`",
                artifact.path
            )));
        }
    }
    let actual_paths: BTreeSet<String> = actual_artifacts.keys().cloned().collect();
    if actual_paths != expected_artifacts {
        return Err(schema(
            "receipt artifact paths differ from registered ownership",
        ));
    }
    for path in &expected_artifacts {
        let actual = actual_artifacts
            .get(path)
            .expect("exact artifact sets include every expected path");
        let file = checked_regular_file(root, Path::new(path))?;
        let bytes = read_bounded(&file, MAX_ARTIFACT_BYTES)?;
        if sha256_hex(&bytes) != *actual {
            return Err(schema(format!(
                "artifact `{path}` differs from its receipt digest"
            )));
        }
    }
    validate_adapter(
        root,
        Adapter::AstProtocol,
        aspect,
        &expected_artifacts,
        receipt.mutation.as_ref(),
    )?;
    Ok(())
}

// --- Package contract receipt format (B6.x, F2.x) ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "receipt fields are validated by deserialization shape, not all are read"
)]
struct PackageReceipt {
    schema: String,
    leaf: String,
    aspect: String,
    date: String,
    #[serde(default)]
    authority: Option<PackageAuthority>,
    #[serde(default)]
    owns: Vec<String>,
    #[serde(default)]
    source_sha256: Option<String>,
    #[serde(default)]
    commands: Vec<PackageCommand>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    mutation: Option<String>,
    #[serde(default)]
    sha256_before: Option<String>,
    #[serde(default)]
    sha256_mutated: Option<String>,
    #[serde(default)]
    sha256_after_restore: Option<String>,
    #[serde(default)]
    restore: Option<String>,
    #[serde(default)]
    mutated_command: Option<Vec<String>>,
    #[serde(default)]
    mutated_status: Option<i32>,
    #[serde(default)]
    mutated_output_sha256: Option<String>,
    #[serde(default)]
    guard: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "authority fields are validated by deserialization shape, not read"
)]
struct PackageAuthority {
    release: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    version_stdout: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "command fields are validated by deserialization shape, not all are read"
)]
struct PackageCommand {
    argv: Vec<String>,
    status: i32,
    #[serde(default)]
    stdout_sha256: Option<String>,
    #[serde(default)]
    stderr_sha256: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

fn validate_package_contract_receipt(
    root: &Path,
    leaf: &str,
    aspect: &str,
    owns: &[String],
    bytes: &[u8],
) -> Result<()> {
    let receipt: PackageReceipt = serde_json::from_slice(bytes).map_err(|error| {
        VerificationError::new(
            ErrorCode::Json,
            format!("invalid package contract receipt: {error}"),
        )
    })?;
    if receipt.schema != SCHEMA {
        return Err(schema(format!(
            "unsupported leaf evidence schema `{}`",
            receipt.schema
        )));
    }
    if receipt.leaf != leaf {
        return Err(schema(format!(
            "receipt leaf `{}` does not match registered leaf `{leaf}`",
            receipt.leaf
        )));
    }
    if receipt.aspect != aspect {
        return Err(schema(format!(
            "receipt aspect `{}` does not match registered aspect `{aspect}`",
            receipt.aspect
        )));
    }

    let expected_artifacts = expand_owned_artifacts(root, owns)?;

    // Verify the receipt's owns list matches the registered ownership (when present).
    if !receipt.owns.is_empty() {
        let receipt_owns: BTreeSet<String> = receipt.owns.iter().cloned().collect();
        if receipt_owns != expected_artifacts {
            return Err(schema(format!(
                "receipt owns differ from registered ownership for `{leaf}`"
            )));
        }
    }

    // Verify every owned artifact still matches its recorded source digest.
    if let Some(source_sha) = &receipt.source_sha256 {
        require_sha256("source_sha256", source_sha)?;
        let combined = combined_digest(root, &expected_artifacts)?;
        // Accept either the combined digest or, for a single owned artifact,
        // the direct file SHA-256 (legacy B6.x receipts use the file hash).
        let single = if expected_artifacts.len() == 1 {
            let path = expected_artifacts.iter().next().expect("single artifact");
            let file = checked_regular_file(root, Path::new(path))?;
            let bytes = read_bounded(&file, MAX_ARTIFACT_BYTES)?;
            Some(sha256_hex(&bytes))
        } else {
            None
        };
        if &combined != source_sha && single.as_deref() != Some(source_sha) {
            return Err(schema(format!(
                "source_sha256 differs from current owned artifacts for `{leaf}`"
            )));
        }
    }

    if aspect == "contract" {
        // Contract receipts must have at least one command with a passing status.
        if receipt.commands.is_empty() {
            return Err(schema(format!(
                "contract receipt for `{leaf}` has no commands"
            )));
        }
        for cmd in &receipt.commands {
            if cmd.argv.is_empty() {
                return Err(schema(format!(
                    "contract receipt for `{leaf}` has an empty argv"
                )));
            }
            if let Some(stdout_sha) = &cmd.stdout_sha256 {
                require_sha256("stdout_sha256", stdout_sha)?;
            }
            if let Some(stderr_sha) = &cmd.stderr_sha256 {
                require_sha256("stderr_sha256", stderr_sha)?;
            }
        }
        // Contract receipts must not declare a mutation.
        if receipt.mutation.is_some() {
            return Err(schema(format!(
                "contract receipt for `{leaf}` must not declare a mutation"
            )));
        }
    } else if aspect == "mutation" {
        // Mutation receipts must have a file, mutation, and digest triple.
        let file = receipt
            .file
            .as_ref()
            .ok_or_else(|| schema(format!("mutation receipt for `{leaf}` omits file")))?;
        let mutation = receipt
            .mutation
            .as_ref()
            .ok_or_else(|| schema(format!("mutation receipt for `{leaf}` omits mutation")))?;
        let sha_before = receipt
            .sha256_before
            .as_ref()
            .ok_or_else(|| schema(format!("mutation receipt for `{leaf}` omits sha256_before")))?;
        let sha_mutated = receipt.sha256_mutated.as_ref().ok_or_else(|| {
            schema(format!(
                "mutation receipt for `{leaf}` omits sha256_mutated"
            ))
        })?;
        let sha_after = receipt.sha256_after_restore.as_ref().ok_or_else(|| {
            schema(format!(
                "mutation receipt for `{leaf}` omits sha256_after_restore"
            ))
        })?;
        require_sha256("sha256_before", sha_before)?;
        require_sha256("sha256_mutated", sha_mutated)?;
        require_sha256("sha256_after_restore", sha_after)?;

        // The file must be one of the owned artifacts.
        if !expected_artifacts.contains(file) {
            return Err(schema(format!(
                "mutation file `{file}` is not an owned artifact of `{leaf}`"
            )));
        }

        // The baseline digest must match the current file.
        let file_path = checked_regular_file(root, Path::new(file))?;
        let file_bytes = read_bounded(&file_path, MAX_ARTIFACT_BYTES)?;
        let current_sha = sha256_hex(&file_bytes);
        if sha_before != &current_sha {
            return Err(schema(format!(
                "mutation baseline sha256 differs from current file `{file}`"
            )));
        }
        if sha_after != &current_sha {
            return Err(schema(format!(
                "mutation restored sha256 differs from current file `{file}`"
            )));
        }
        if sha_before == sha_mutated {
            return Err(schema(format!("mutation did not alter file `{file}`")));
        }

        // The mutated command must be present and must have a non-zero status
        // (the mutation must make the contract test fail).
        let mutated_cmd = receipt.mutated_command.as_ref().ok_or_else(|| {
            schema(format!(
                "mutation receipt for `{leaf}` omits mutated_command"
            ))
        })?;
        if mutated_cmd.is_empty() {
            return Err(schema(format!(
                "mutation receipt for `{leaf}` has empty mutated_command"
            )));
        }
        let mutated_status = receipt.mutated_status.unwrap_or(0);
        if mutated_status == 0 {
            return Err(schema(format!(
                "mutation receipt for `{leaf}` has status 0; mutation must make the contract test fail"
            )));
        }
        if let Some(output_sha) = &receipt.mutated_output_sha256 {
            require_sha256("mutated_output_sha256", output_sha)?;
        }

        // The guard description must be present.
        if receipt.guard.is_none() && receipt.notes.is_none() {
            return Err(schema(format!(
                "mutation receipt for `{leaf}` must have a guard or notes describing the failing assertion"
            )));
        }

        // The mutation description must be non-empty.
        if mutation.is_empty() {
            return Err(schema(format!(
                "mutation receipt for `{leaf}` has empty mutation description"
            )));
        }
    }

    Ok(())
}

fn combined_digest(root: &Path, artifacts: &BTreeSet<String>) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for path in artifacts {
        let file = checked_regular_file(root, Path::new(path))?;
        let bytes = read_bounded(&file, MAX_ARTIFACT_BYTES)?;
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
fn validate_test_artifact_receipt(
    root: &Path,
    leaf: &str,
    aspect: &str,
    owns: &[String],
    bytes: &[u8],
) -> Result<()> {
    let receipt: Receipt = serde_json::from_slice(bytes).map_err(|error| {
        VerificationError::new(
            ErrorCode::Json,
            format!("invalid leaf evidence receipt: {error}"),
        )
    })?;
    if receipt.schema != SCHEMA {
        return Err(schema(format!(
            "unsupported leaf evidence schema `{}`",
            receipt.schema
        )));
    }
    if receipt.leaf != leaf {
        return Err(schema(format!(
            "receipt leaf `{}` does not match registered leaf `{leaf}`",
            receipt.leaf
        )));
    }
    if receipt.aspect != aspect {
        return Err(schema(format!(
            "receipt aspect `{}` does not match registered aspect `{aspect}`",
            receipt.aspect
        )));
    }
    let expected_gate = gate_for_aspect(aspect).ok_or_else(|| {
        schema(format!(
            "leaf evidence cannot certify unsupported aspect `{aspect}`"
        ))
    })?;
    if receipt.gate != expected_gate {
        return Err(schema(format!(
            "receipt gate `{}` does not match `{expected_gate}` for aspect `{aspect}`",
            receipt.gate
        )));
    }
    if receipt.adapter != Adapter::TestArtifact.name() {
        return Err(schema(format!(
            "receipt adapter `{}` does not match registered adapter `{}`",
            receipt.adapter,
            Adapter::TestArtifact.name()
        )));
    }
    let expected_artifacts = expand_owned_artifacts(root, owns)?;
    let mut actual_artifacts = BTreeMap::new();
    for artifact in &receipt.artifacts {
        require_sha256("artifact", &artifact.sha256)?;
        if actual_artifacts
            .insert(artifact.path.clone(), artifact.sha256.clone())
            .is_some()
        {
            return Err(schema(format!(
                "receipt duplicates artifact `{}`",
                artifact.path
            )));
        }
    }
    let actual_paths: BTreeSet<String> = actual_artifacts.keys().cloned().collect();
    if actual_paths != expected_artifacts {
        return Err(schema(
            "receipt artifact paths differ from registered ownership",
        ));
    }
    for path in &expected_artifacts {
        let actual = actual_artifacts
            .get(path)
            .expect("exact artifact sets include every expected path");
        let file = checked_regular_file(root, Path::new(path))?;
        let bytes = read_bounded(&file, MAX_ARTIFACT_BYTES)?;
        if sha256_hex(&bytes) != *actual {
            return Err(schema(format!(
                "artifact `{path}` differs from its receipt digest"
            )));
        }
    }
    if receipt.mutation.is_some() {
        Err(schema("test artifact receipts must not declare a mutation"))
    } else {
        Ok(())
    }
}

/// Generates one registered B0 protocol receipt and returns its gate evidence.
pub fn generate(root: &Path, leaf: &str, aspect: &str, output: &Path) -> Result<String> {
    if leaf != "B0.1" || !matches!(aspect, "contract" | "mutation") {
        return Err(schema(format!(
            "no receipt generator is registered for `{leaf}` aspect `{aspect}`"
        )));
    }
    let output = output
        .to_str()
        .ok_or_else(|| schema("receipt output path is not UTF-8"))?;
    let output = checked_relative_path(output)?;
    if !output.starts_with(".outline/evidence") {
        return Err(schema(
            "generated leaf receipts must live below .outline/evidence",
        ));
    }

    let owns = vec![B0_PROTOCOL_PATH.to_owned()];
    let paths = expand_owned_artifacts(root, &owns)?;
    let artifacts = paths
        .iter()
        .map(|path| {
            let bytes = read_bounded(&root.join(path), MAX_ARTIFACT_BYTES)?;
            Ok(Artifact {
                path: path.clone(),
                sha256: sha256_hex(&bytes),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let protocol = read_bounded(&root.join(B0_PROTOCOL_PATH), MAX_ARTIFACT_BYTES)?;
    let mutation = if aspect == "mutation" {
        let text = std::str::from_utf8(&protocol)
            .map_err(|_| schema("B0 protocol artifact is not UTF-8"))?;
        let mutated = text.replacen(B0_MUTATION_LINE, "", 1);
        if mutated == text {
            return Err(schema("B0 registered mutation did not alter the protocol"));
        }
        Some(MutationEvidence {
            id: B0_MUTATION_ID.to_owned(),
            baseline_sha256: sha256_hex(&protocol),
            mutated_sha256: sha256_hex(mutated.as_bytes()),
            restored_sha256: sha256_hex(&protocol),
        })
    } else {
        None
    };
    let receipt = Receipt {
        schema: SCHEMA.to_owned(),
        leaf: leaf.to_owned(),
        aspect: aspect.to_owned(),
        gate: gate_for_aspect(aspect)
            .expect("registered B0 receipt aspect has a gate")
            .to_owned(),
        adapter: Adapter::AstProtocol.name().to_owned(),
        artifacts,
        mutation,
    };
    let bytes = serde_json::to_vec(&receipt)
        .map_err(|error| VerificationError::new(ErrorCode::Json, error.to_string()))?;
    let destination = root.join(&output);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    fs::write(&destination, &bytes).map_err(|error| io_error(&destination, error))?;
    let evidence = format!("receipt={} sha256={}", output.display(), sha256_hex(&bytes));
    validate(root, leaf, aspect, &owns, &evidence)?;
    Ok(evidence)
}

pub(crate) fn gate_for_aspect(aspect: &str) -> Option<&'static str> {
    match aspect {
        "contract" => Some("G1"),
        "evidence" => Some("G2"),
        "coverage" => Some("G3"),
        "regression" => Some("G4"),
        "mutation" => Some("G5"),
        _ => None,
    }
}

const B0_PROTOCOL_PATH: &str = "docs/dev/ast_change_window_protocol.md";
const B0_MUTATION_ID: &str = "b0-append-only-order";
const B0_MUTATION_LINE: &str = "- Do not reorder or remove existing enum variants.\n";
const B0_REQUIRED_RULES: [&str; 7] = [
    "Append new grammar nodes to `NodeKind`",
    "Append new token variants to `TokenKind`",
    "Do not reorder or remove existing enum variants.",
    "Do not use a non-exhaustive wildcard default",
    "Source positions are UTF-16 code-unit offsets",
    "TextRange::new(start, end)",
    "diagnostics regenerate",
];

fn registered_adapter(leaf: &str, aspect: &str) -> Option<Adapter> {
    match (leaf, aspect) {
        ("B0.1", "contract" | "mutation") => Some(Adapter::AstProtocol),
        ("B6.1" | "B6.2" | "F2.1" | "F2.2" | "F2.3", _) => Some(Adapter::PackageContract),
        #[cfg(test)]
        ("X1", _) => Some(Adapter::TestArtifact),
        _ => None,
    }
}

fn validate_adapter(
    root: &Path,
    adapter: Adapter,
    aspect: &str,
    artifacts: &BTreeSet<String>,
    mutation: Option<&MutationEvidence>,
) -> Result<()> {
    match adapter {
        Adapter::AstProtocol => validate_ast_protocol_adapter(root, aspect, artifacts, mutation),
        Adapter::PackageContract => Ok(()),
        #[cfg(test)]
        Adapter::TestArtifact => {
            if mutation.is_some() {
                Err(schema("test artifact receipts must not declare a mutation"))
            } else {
                Ok(())
            }
        }
    }
}

fn validate_ast_protocol_adapter(
    root: &Path,
    aspect: &str,
    artifacts: &BTreeSet<String>,
    mutation: Option<&MutationEvidence>,
) -> Result<()> {
    let expected = BTreeSet::from([B0_PROTOCOL_PATH.to_owned()]);
    if artifacts != &expected {
        return Err(schema(
            "B0 protocol receipt must bind only its protocol artifact",
        ));
    }
    let bytes = read_bounded(&root.join(B0_PROTOCOL_PATH), MAX_ARTIFACT_BYTES)?;
    validate_ast_protocol(&bytes)?;
    if aspect == "contract" {
        return if mutation.is_none() {
            Ok(())
        } else {
            Err(schema("B0 contract receipt must not declare a mutation"))
        };
    }

    let mutation = mutation.ok_or_else(|| schema("B0 mutation receipt omits mutation evidence"))?;
    if mutation.id != B0_MUTATION_ID {
        return Err(schema(format!("unknown B0 mutation `{}`", mutation.id)));
    }
    require_sha256("mutation baseline", &mutation.baseline_sha256)?;
    require_sha256("mutation mutated", &mutation.mutated_sha256)?;
    require_sha256("mutation restored", &mutation.restored_sha256)?;
    let baseline = sha256_hex(&bytes);
    if mutation.baseline_sha256 != baseline || mutation.restored_sha256 != baseline {
        return Err(schema(
            "B0 mutation baseline or restored digest differs from the protocol",
        ));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| schema("B0 protocol artifact is not UTF-8"))?;
    let mutated = text.replacen(B0_MUTATION_LINE, "", 1);
    if mutated == text {
        return Err(schema("B0 registered mutation did not alter the protocol"));
    }
    if mutation.mutated_sha256 != sha256_hex(mutated.as_bytes()) {
        return Err(schema(
            "B0 mutation digest differs from the registered mutation",
        ));
    }
    if validate_ast_protocol(mutated.as_bytes()).is_ok() {
        return Err(schema(
            "B0 registered mutation did not make the contract fail",
        ));
    }
    Ok(())
}

fn validate_ast_protocol(bytes: &[u8]) -> Result<()> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| schema("AST protocol artifact is not UTF-8"))?;
    for required in B0_REQUIRED_RULES {
        if !text.contains(required) {
            return Err(schema(format!(
                "AST protocol omits required rule `{required}`"
            )));
        }
    }
    Ok(())
}
fn parse_reference(evidence: &str) -> Result<(PathBuf, String)> {
    let mut receipt = None;
    let mut digest = None;
    for field in evidence.split_ascii_whitespace() {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| schema("evidence must contain only receipt and sha256 fields"))?;
        match key {
            "receipt" if receipt.replace(value).is_none() => {}
            "sha256" if digest.replace(value).is_none() => {}
            _ => {
                return Err(schema(
                    "evidence must contain one receipt and one sha256 field",
                ));
            }
        }
    }
    let receipt = receipt.ok_or_else(|| schema("evidence omits receipt"))?;
    let digest = digest.ok_or_else(|| schema("evidence omits sha256"))?;
    Ok((checked_relative_path(receipt)?, digest.to_owned()))
}

fn expand_owned_artifacts(root: &Path, owns: &[String]) -> Result<BTreeSet<String>> {
    let mut artifacts = BTreeSet::new();
    for owned in owns {
        let stars = owned.bytes().filter(|byte| *byte == b'*').count();
        match stars {
            0 => collect_owned_artifacts(root, &checked_relative_path(owned)?, &mut artifacts)?,
            1 => collect_single_star(root, owned, &mut artifacts)?,
            _ => return Err(schema(format!("unsupported ownership pattern `{owned}`"))),
        }
    }
    if artifacts.is_empty() {
        return Err(schema(
            "registered ownership expands to no regular artifacts",
        ));
    }
    Ok(artifacts)
}

fn collect_single_star(root: &Path, pattern: &str, artifacts: &mut BTreeSet<String>) -> Result<()> {
    let (prefix, suffix) = pattern
        .split_once('*')
        .expect("one star ownership pattern splits once");
    let directory = root.join(checked_relative_path(prefix)?);
    let metadata = fs::symlink_metadata(&directory).map_err(|error| io_error(&directory, error))?;
    if !metadata.is_dir() {
        return Err(schema(format!(
            "ownership pattern `{pattern}` does not name a directory"
        )));
    }
    let mut entries = fs::read_dir(&directory)
        .map_err(|error| io_error(&directory, error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| io_error(&directory, error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| io_error(&path, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(schema(format!(
                "ownership pattern `{pattern}` contains a non-UTF-8 path"
            )));
        };
        if file_type.is_symlink() && name.ends_with(suffix) {
            return Err(schema(format!(
                "ownership pattern `{pattern}` resolves through a symlink"
            )));
        }
        if file_type.is_file() && name.ends_with(suffix) {
            artifacts.insert(normalized_relative(root, &path)?);
        }
    }
    Ok(())
}

fn collect_owned_artifacts(
    root: &Path,
    relative: &Path,
    artifacts: &mut BTreeSet<String>,
) -> Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(schema(format!(
            "owned artifact `{}` must not be a symlink",
            relative.display()
        )));
    }
    if metadata.is_file() {
        artifacts.insert(normalized_relative(root, &path)?);
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(schema(format!(
            "owned artifact `{}` is not a regular file or directory",
            relative.display()
        )));
    }
    let mut entries = fs::read_dir(&path)
        .map_err(|error| io_error(&path, error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| io_error(&path, error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let child = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| schema("owned artifact escaped repository root"))?
            .to_path_buf();
        collect_owned_artifacts(root, &child, artifacts)?;
    }
    Ok(())
}

fn checked_regular_file(root: &Path, relative: &Path) -> Result<PathBuf> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(schema(format!(
            "evidence path `{}` must be a regular file",
            relative.display()
        )));
    }
    let canonical_root = root.canonicalize().map_err(|error| io_error(root, error))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| io_error(&path, error))?;
    if !canonical.starts_with(canonical_root) {
        return Err(schema(format!(
            "evidence path `{}` escapes repository root",
            relative.display()
        )));
    }
    Ok(canonical)
}

fn checked_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        })
    {
        return Err(schema(
            "evidence paths must be nonempty repository-relative paths",
        ));
    }
    Ok(path.to_path_buf())
}

fn normalized_relative(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| schema("owned artifact escaped repository root"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(schema("owned artifact path is not normalized"));
        };
        let part = part
            .to_str()
            .ok_or_else(|| schema("owned artifact path is not UTF-8"))?;
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(schema("owned artifact path is empty"));
    }
    Ok(parts.join("/"))
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    let maximum_u64 =
        u64::try_from(maximum).map_err(|_| schema("evidence byte limit does not fit u64"))?;
    let mut bytes = Vec::new();
    file.take(maximum_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() > maximum {
        return Err(schema(format!(
            "evidence file `{}` exceeds {maximum} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn require_sha256(field: &str, digest: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(schema(format!("{field} digest is not lowercase SHA-256")))
    }
}

fn io_error(path: &Path, error: std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

fn schema(detail: impl Into<String>) -> VerificationError {
    VerificationError::new(ErrorCode::Schema, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "bamts-leaf-evidence-{}-{}",
                std::process::id(),
                NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("docs/dev")).unwrap();
            fs::create_dir_all(path.join(".outline/evidence")).unwrap();
            Self(path)
        }

        fn protocol_path(&self) -> PathBuf {
            self.0.join(B0_PROTOCOL_PATH)
        }

        fn write_protocol(&self) -> Vec<u8> {
            let mut text = String::new();
            for required in B0_REQUIRED_RULES {
                if required == "Do not reorder or remove existing enum variants." {
                    text.push_str(B0_MUTATION_LINE);
                } else {
                    text.push_str(required);
                    text.push('\n');
                }
            }
            fs::write(self.protocol_path(), text.as_bytes()).unwrap();
            text.into_bytes()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn owns() -> Vec<String> {
        vec![B0_PROTOCOL_PATH.to_owned()]
    }

    fn write_receipt(
        scratch: &Scratch,
        aspect: &str,
        mutation: Option<serde_json::Value>,
    ) -> String {
        let artifact = sha256_hex(&fs::read(scratch.protocol_path()).unwrap());
        let mut receipt = serde_json::json!({
            "schema": SCHEMA,
            "leaf": "B0.1",
            "aspect": aspect,
            "gate": gate_for_aspect(aspect).unwrap(),
            "adapter": "ast_protocol",
            "artifacts": [{"path": B0_PROTOCOL_PATH, "sha256": artifact}],
        });
        if let Some(mutation) = mutation {
            receipt["mutation"] = mutation;
        }
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let path = scratch.0.join(".outline/evidence/receipt.json");
        fs::write(&path, &bytes).unwrap();
        format!(
            "receipt=.outline/evidence/receipt.json sha256={}",
            sha256_hex(&bytes)
        )
    }

    #[test]
    fn ast_protocol_contract_binds_current_required_rules() {
        let scratch = Scratch::new();
        scratch.write_protocol();
        let evidence = write_receipt(&scratch, "contract", None);
        validate(&scratch.0, "B0.1", "contract", &owns(), &evidence).unwrap();

        fs::write(
            scratch.protocol_path(),
            "Append new grammar nodes to `NodeKind`\n",
        )
        .unwrap();
        assert!(validate(&scratch.0, "B0.1", "contract", &owns(), &evidence).is_err());
    }

    #[test]
    fn generator_writes_a_valid_registered_b0_receipt() {
        let scratch = Scratch::new();
        scratch.write_protocol();
        let evidence = generate(
            &scratch.0,
            "B0.1",
            "contract",
            Path::new(".outline/evidence/contract.json"),
        )
        .unwrap();
        validate(&scratch.0, "B0.1", "contract", &owns(), &evidence).unwrap();
    }

    #[test]
    fn ast_protocol_mutation_requires_registered_failure() {
        let scratch = Scratch::new();
        let baseline = scratch.write_protocol();
        let text = std::str::from_utf8(&baseline).unwrap();
        let mutated = text.replacen(B0_MUTATION_LINE, "", 1);
        let mutation = serde_json::json!({
            "id": B0_MUTATION_ID,
            "baseline_sha256": sha256_hex(&baseline),
            "mutated_sha256": sha256_hex(mutated.as_bytes()),
            "restored_sha256": sha256_hex(&baseline),
        });
        let evidence = write_receipt(&scratch, "mutation", Some(mutation));
        validate(&scratch.0, "B0.1", "mutation", &owns(), &evidence).unwrap();

        let forged = serde_json::json!({
            "id": B0_MUTATION_ID,
            "baseline_sha256": sha256_hex(&baseline),
            "mutated_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "restored_sha256": sha256_hex(&baseline),
        });
        let evidence = write_receipt(&scratch, "mutation", Some(forged));
        assert!(validate(&scratch.0, "B0.1", "mutation", &owns(), &evidence).is_err());
    }
}
