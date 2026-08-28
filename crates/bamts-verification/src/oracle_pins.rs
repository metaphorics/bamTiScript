//! Exact TypeScript 7.0.2 oracle identity verification.
//!
//! A conformance run is release-valid only when every pin matches:
//! npm `typescript@7.0.2` with its sha512 integrity, compiler commit
//! `microsoft/typescript-go@2bd066d…` at tag `typescript/v7.0.2`, and suite
//! gitlink `microsoft/TypeScript@4d4f005c…`.
//!
//! Evidence is read from the existing vendor catalog (`vendor/sources.toml`)
//! plus root npm manifests (`package.json`, `package-lock.json`, and
//! `node_modules/typescript/package.json`). Branch names are never accepted
//! as provenance. The published package `repository` field identifies the
//! npm distribution metadata only and never substitutes for the compiler pin.

use std::{collections::BTreeMap, fs, path::Path};

use serde::Deserialize;
use serde_json::Value;
use toml::Table;

use crate::{
    ErrorCode, Result, VerificationError,
    schema::{TYPESCRIPT_COMPILER_SOURCE, TYPESCRIPT_NPM_SOURCE, TYPESCRIPT_SUITE_SOURCE},
};

/// Exact npm package name.
pub const NPM_NAME: &str = "typescript";
/// Exact npm package version.
pub const NPM_VERSION: &str = "7.0.2";
/// Exact npm package specifier `name@version`.
pub const NPM_SPECIFIER: &str = "typescript@7.0.2";
/// Pinned sha512 integrity for `typescript@7.0.2` from the npm lockfile.
pub const NPM_INTEGRITY: &str = "sha512-8FYau96o3NKOhbjKi/qNvG/W5jhzxkbdm5sj9AbZ/5T5sWqn3hJgLfGx27sRKZWTvyzCP8dLRBTf5tBTSRVUNA==";
/// Exact npm registry tarball URL.
pub const NPM_URL: &str = "https://registry.npmjs.org/typescript/-/typescript-7.0.2.tgz";
/// Digest algorithm for the npm tarball source record.
pub const NPM_DIGEST_ALGORITHM: &str = "sha512";
/// Digest value for the npm tarball source record (no algorithm prefix).
pub const NPM_DIGEST: &str =
    "8FYau96o3NKOhbjKi/qNvG/W5jhzxkbdm5sj9AbZ/5T5sWqn3hJgLfGx27sRKZWTvyzCP8dLRBTf5tBTSRVUNA==";

/// Compiler source repository. Never derived from the published `repository` field.
pub const COMPILER_REPOSITORY: &str = "https://github.com/microsoft/typescript-go";
/// Immutable compiler commit for tag `typescript/v7.0.2`.
pub const COMPILER_COMMIT: &str = "2bd066d87f5bafd315be9f40889d0a60b9e58e0b";
/// Official release tag. Not `v7.0.2`.
pub const COMPILER_TAG: &str = "typescript/v7.0.2";
/// Exact compiler archive URL at the pinned commit.
pub const COMPILER_URL: &str = "https://github.com/microsoft/typescript-go/archive/2bd066d87f5bafd315be9f40889d0a60b9e58e0b.tar.gz";
/// Digest algorithm for the compiler archive source record.
pub const COMPILER_DIGEST_ALGORITHM: &str = "sha256";
/// Digest value for the compiler archive source record.
pub const COMPILER_DIGEST: &str =
    "5ccb47dbb3f68cd0da58b71e6f445eee36a50bd3e9f9f330cc23a97b88500119";

/// Suite / baseline repository.
pub const SUITE_REPOSITORY: &str = "https://github.com/microsoft/TypeScript";
/// Immutable TypeScript suite gitlink at the compiler revision.
pub const SUITE_COMMIT: &str = "4d4f005c8541e0255a9d8791205fdce326e462bc";
/// Exact suite archive URL at the pinned commit.
pub const SUITE_URL: &str = "https://github.com/microsoft/TypeScript/archive/4d4f005c8541e0255a9d8791205fdce326e462bc.tar.gz";
/// Digest algorithm for the suite archive source record.
pub const SUITE_DIGEST_ALGORITHM: &str = "sha256";
/// Digest value for the suite archive source record.
pub const SUITE_DIGEST: &str = "99cbcb9abf5308b15e90b991919b3f897918f5c19c13dce3a4d3cab618c64bf9";

/// Published npm package `repository.url`. Not compiler provenance.
pub const PUBLISHED_REPOSITORY_URL: &str = "https://github.com/microsoft/TypeScript.git";

const COMMIT_LEN: usize = 40;
const SOURCES_PATH: &str = "vendor/sources.toml";
const PACKAGE_JSON_PATH: &str = "package.json";
const PACKAGE_LOCK_PATH: &str = "package-lock.json";
const TYPESCRIPT_PACKAGE_PATH: &str = "node_modules/typescript/package.json";
const LOCK_PACKAGE_KEY: &str = "node_modules/typescript";
const SOURCE_NPM: &str = TYPESCRIPT_NPM_SOURCE;
const SOURCE_COMPILER: &str = TYPESCRIPT_COMPILER_SOURCE;
const SOURCE_SUITE: &str = TYPESCRIPT_SUITE_SOURCE;
const SOURCE_KIND_NPM: &str = "npm";
const SOURCE_KIND_GIT_ARCHIVE: &str = "git-archive";

/// The four-part TypeScript 7.0.2 oracle identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OraclePins {
    pub npm_specifier: String,
    pub npm_integrity: String,
    pub compiler_repository: String,
    pub compiler_commit: String,
    pub compiler_tag: String,
    pub suite_repository: String,
    pub suite_commit: String,
}

impl OraclePins {
    /// The canonical release pins.
    pub fn expected() -> Self {
        Self {
            npm_specifier: NPM_SPECIFIER.to_owned(),
            npm_integrity: NPM_INTEGRITY.to_owned(),
            compiler_repository: COMPILER_REPOSITORY.to_owned(),
            compiler_commit: COMPILER_COMMIT.to_owned(),
            compiler_tag: COMPILER_TAG.to_owned(),
            suite_repository: SUITE_REPOSITORY.to_owned(),
            suite_commit: SUITE_COMMIT.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
struct SourceRecord {
    name: String,
    kind: String,
    pin: String,
    url: String,
    digest_algorithm: String,
    digest: String,
    commit: String,
    repository: String,
}

#[derive(Debug, Clone)]
struct NpmManifests {
    root_declaration: String,
    lock_root_declaration: String,
    lock_version: String,
    lock_integrity: String,
    package_name: String,
    package_version: String,
    package_git_head: String,
    package_repository_url: String,
}

#[derive(Deserialize)]
struct SourcesDocument {
    #[serde(default)]
    source: Vec<Table>,
}

struct SourceExpectation {
    kind: &'static str,
    pin: &'static str,
    url: &'static str,
    digest_algorithm: &'static str,
    digest: &'static str,
    commit: &'static str,
    repository: Option<&'static str>,
}

/// Read vendor + npm evidence and prove it matches the TypeScript 7.0.2 oracle.
pub fn verify_oracle_pins(root: &Path) -> Result<OraclePins> {
    let sources = load_oracle_sources(root)?;
    let manifests = load_npm_manifests(root)?;
    let observed = observe_pins(&sources, &manifests)?;

    verify_source_record(
        &sources.npm,
        &SourceExpectation {
            kind: SOURCE_KIND_NPM,
            pin: NPM_VERSION,
            url: NPM_URL,
            digest_algorithm: NPM_DIGEST_ALGORITHM,
            digest: NPM_DIGEST,
            commit: COMPILER_COMMIT,
            repository: None,
        },
    )?;
    verify_source_record(
        &sources.compiler,
        &SourceExpectation {
            kind: SOURCE_KIND_GIT_ARCHIVE,
            pin: COMPILER_TAG,
            url: COMPILER_URL,
            digest_algorithm: COMPILER_DIGEST_ALGORITHM,
            digest: COMPILER_DIGEST,
            commit: COMPILER_COMMIT,
            repository: Some(COMPILER_REPOSITORY),
        },
    )?;
    verify_source_record(
        &sources.suite,
        &SourceExpectation {
            kind: SOURCE_KIND_GIT_ARCHIVE,
            pin: SUITE_COMMIT,
            url: SUITE_URL,
            digest_algorithm: SUITE_DIGEST_ALGORITHM,
            digest: SUITE_DIGEST,
            commit: SUITE_COMMIT,
            repository: Some(SUITE_REPOSITORY),
        },
    )?;
    verify_npm_manifests(&manifests, &sources.npm)?;
    verify_pin_identity(&observed)?;
    Ok(observed)
}

fn observe_pins(sources: &OracleSources, manifests: &NpmManifests) -> Result<OraclePins> {
    reject_moving_ref("compiler commit", &sources.compiler.commit, RefKind::Commit)?;
    reject_moving_ref("suite commit", &sources.suite.commit, RefKind::Commit)?;
    reject_moving_ref(
        "compiler tag",
        &sources.compiler.pin,
        RefKind::Tag(COMPILER_TAG),
    )?;
    reject_repository_substitution(&sources.compiler.repository)?;

    Ok(OraclePins {
        npm_specifier: format!("{}@{}", manifests.package_name, manifests.lock_version),
        npm_integrity: manifests.lock_integrity.clone(),
        compiler_repository: sources.compiler.repository.clone(),
        compiler_commit: sources.compiler.commit.clone(),
        compiler_tag: sources.compiler.pin.clone(),
        suite_repository: sources.suite.repository.clone(),
        suite_commit: sources.suite.commit.clone(),
    })
}

struct OracleSources {
    npm: SourceRecord,
    compiler: SourceRecord,
    suite: SourceRecord,
}

fn load_oracle_sources(root: &Path) -> Result<OracleSources> {
    let path = root.join(SOURCES_PATH);
    let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
    let document: SourcesDocument = parse_toml(&path, &bytes)?;

    // Index by name first. Only the three TypeScript 7 oracle records are
    // identity-validated here; unrelated catalog rows may omit commit/kind.
    let mut by_name = BTreeMap::new();
    for table in document.source {
        let Some(name) = table
            .get("name")
            .and_then(toml::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        let is_oracle = matches!(name.as_str(), SOURCE_NPM | SOURCE_COMPILER | SOURCE_SUITE);
        if by_name.insert(name.clone(), table).is_some() && is_oracle {
            return Err(provenance_mismatch(format!(
                "{}: duplicate source `{name}`",
                path.display()
            )));
        }
    }

    Ok(OracleSources {
        npm: take_source(&path, &mut by_name, SOURCE_NPM)?,
        compiler: take_source(&path, &mut by_name, SOURCE_COMPILER)?,
        suite: take_source(&path, &mut by_name, SOURCE_SUITE)?,
    })
}

fn take_source(
    path: &Path,
    by_name: &mut BTreeMap<String, Table>,
    name: &str,
) -> Result<SourceRecord> {
    let table = by_name.remove(name).ok_or_else(|| {
        provenance_mismatch(format!("{}: missing source `{name}`", path.display()))
    })?;
    parse_source_record(path, table)
}

fn parse_source_record(path: &Path, table: Table) -> Result<SourceRecord> {
    let name = require_table_string(path, &table, "name", "source")?;
    let pin = require_named_string(path, &table, "pin", &name)?;
    let url = require_named_string(path, &table, "url", &name)?;
    let digest_algorithm = require_named_string(path, &table, "digest_algorithm", &name)?;
    let digest = require_named_string(path, &table, "digest", &name)?;
    let commit = require_named_string(path, &table, "commit", &name)?;
    let kind = match table.get("kind") {
        None => infer_source_kind(&url)?,
        Some(value) => require_value_string(path, value, &format!("source `{name}` kind"))?,
    };
    let repository = match table.get("repository") {
        None => infer_repository(&name, &kind, &url)?,
        Some(value) => require_value_string(path, value, &format!("source `{name}` repository"))?,
    };

    Ok(SourceRecord {
        name,
        kind,
        pin,
        url,
        digest_algorithm,
        digest,
        commit,
        repository,
    })
}

fn infer_source_kind(url: &str) -> Result<String> {
    if url.starts_with("https://registry.npmjs.org/") {
        Ok(SOURCE_KIND_NPM.to_owned())
    } else if url.starts_with("https://github.com/") && url.contains("/archive/") {
        Ok(SOURCE_KIND_GIT_ARCHIVE.to_owned())
    } else {
        Err(provenance_mismatch(format!(
            "unable to infer source kind from url `{url}`"
        )))
    }
}

fn infer_repository(name: &str, kind: &str, url: &str) -> Result<String> {
    match kind {
        SOURCE_KIND_NPM => Ok(String::new()),
        SOURCE_KIND_GIT_ARCHIVE => github_repository_from_archive_url(url),
        other => Err(provenance_mismatch(format!(
            "source `{name}` has unsupported kind `{other}`"
        ))),
    }
}

fn github_repository_from_archive_url(url: &str) -> Result<String> {
    const PREFIX: &str = "https://github.com/";
    const MARKER: &str = "/archive/";
    let Some(rest) = url.strip_prefix(PREFIX) else {
        return Err(provenance_mismatch(format!(
            "git-archive url must start with `{PREFIX}`, found `{url}`"
        )));
    };
    let Some((repo_path, _)) = rest.split_once(MARKER) else {
        return Err(provenance_mismatch(format!(
            "git-archive url must contain `{MARKER}`, found `{url}`"
        )));
    };
    let mut parts = repo_path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() => {
            Ok(format!("{PREFIX}{owner}/{repo}"))
        }
        _ => Err(provenance_mismatch(format!(
            "git-archive url has malformed repository path `{url}`"
        ))),
    }
}

fn verify_source_record(record: &SourceRecord, expected: &SourceExpectation) -> Result<()> {
    require_pin_match(
        &format!("source `{}` kind", record.name),
        &record.kind,
        expected.kind,
    )?;
    require_pin_match(
        &format!("source `{}` pin", record.name),
        &record.pin,
        expected.pin,
    )?;
    require_pin_match(
        &format!("source `{}` url", record.name),
        &record.url,
        expected.url,
    )?;
    require_pin_match(
        &format!("source `{}` digest_algorithm", record.name),
        &record.digest_algorithm,
        expected.digest_algorithm,
    )?;
    require_pin_match(
        &format!("source `{}` digest", record.name),
        &record.digest,
        expected.digest,
    )?;
    reject_moving_ref(
        &format!("source `{}` commit", record.name),
        &record.commit,
        RefKind::Commit,
    )?;
    require_pin_match(
        &format!("source `{}` commit", record.name),
        &record.commit,
        expected.commit,
    )?;
    if let Some(repository) = expected.repository {
        require_pin_match(
            &format!("source `{}` repository", record.name),
            &record.repository,
            repository,
        )?;
    }
    Ok(())
}

fn load_npm_manifests(root: &Path) -> Result<NpmManifests> {
    let package_json_path = root.join(PACKAGE_JSON_PATH);
    let lock_path = root.join(PACKAGE_LOCK_PATH);
    let installed_path = root.join(TYPESCRIPT_PACKAGE_PATH);

    let package_json = read_json_object(&package_json_path)?;
    let lock = read_json_object(&lock_path)?;
    let installed = read_json_object(&installed_path)?;

    let root_declaration = require_dependency_declaration(&package_json_path, &package_json)?;
    let lock_root_declaration = lock_root_declaration(&lock_path, &lock)?;
    let (lock_version, lock_integrity) = lock_typescript_identity(&lock_path, &lock)?;
    let package_name = require_identity_string(&installed_path, &installed, "name")?;
    let package_version = require_identity_string(&installed_path, &installed, "version")?;
    let package_git_head = require_identity_string(&installed_path, &installed, "gitHead")?;
    let package_repository_url = require_installed_repository_url(&installed_path, &installed)?;

    Ok(NpmManifests {
        root_declaration,
        lock_root_declaration,
        lock_version,
        lock_integrity,
        package_name,
        package_version,
        package_git_head,
        package_repository_url,
    })
}

fn verify_npm_manifests(manifests: &NpmManifests, npm_source: &SourceRecord) -> Result<()> {
    require_pin_match(
        "package.json typescript declaration",
        &manifests.root_declaration,
        NPM_VERSION,
    )?;
    require_pin_match(
        "package-lock root typescript declaration",
        &manifests.lock_root_declaration,
        NPM_VERSION,
    )?;
    require_pin_match(
        "lockfile typescript version",
        &manifests.lock_version,
        NPM_VERSION,
    )?;
    require_pin_match(
        "lockfile typescript integrity",
        &manifests.lock_integrity,
        NPM_INTEGRITY,
    )?;
    require_pin_match(
        "installed typescript name",
        &manifests.package_name,
        NPM_NAME,
    )?;
    require_pin_match(
        "installed typescript version",
        &manifests.package_version,
        NPM_VERSION,
    )?;
    reject_moving_ref(
        "installed typescript gitHead",
        &manifests.package_git_head,
        RefKind::Commit,
    )?;
    require_pin_match(
        "installed typescript gitHead",
        &manifests.package_git_head,
        COMPILER_COMMIT,
    )?;
    require_pin_match(
        "installed typescript repository.url",
        &manifests.package_repository_url,
        PUBLISHED_REPOSITORY_URL,
    )?;

    let expected_integrity = format!("sha512-{}", npm_source.digest);
    require_pin_match(
        "npm source digest vs lockfile integrity",
        &expected_integrity,
        &manifests.lock_integrity,
    )?;
    require_pin_match(
        "npm source pin vs lockfile version",
        &npm_source.pin,
        &manifests.lock_version,
    )?;
    require_pin_match(
        "npm source commit vs installed gitHead",
        &npm_source.commit,
        &manifests.package_git_head,
    )?;

    if manifests.lock_version != manifests.package_version {
        return Err(provenance_mismatch(format!(
            "npm package version drift: lockfile `{}` vs installed `{}`",
            manifests.lock_version, manifests.package_version
        )));
    }
    Ok(())
}

fn verify_pin_identity(pins: &OraclePins) -> Result<()> {
    reject_moving_ref("compiler commit", &pins.compiler_commit, RefKind::Commit)?;
    reject_moving_ref("suite commit", &pins.suite_commit, RefKind::Commit)?;
    reject_moving_ref(
        "compiler tag",
        &pins.compiler_tag,
        RefKind::Tag(COMPILER_TAG),
    )?;
    reject_repository_substitution(&pins.compiler_repository)?;

    let expected = OraclePins::expected();
    require_pin_match(
        "npm specifier",
        &pins.npm_specifier,
        &expected.npm_specifier,
    )?;
    require_pin_match(
        "npm integrity",
        &pins.npm_integrity,
        &expected.npm_integrity,
    )?;
    require_pin_match(
        "compiler repository",
        &pins.compiler_repository,
        &expected.compiler_repository,
    )?;
    require_pin_match(
        "compiler commit",
        &pins.compiler_commit,
        &expected.compiler_commit,
    )?;
    require_pin_match("compiler tag", &pins.compiler_tag, &expected.compiler_tag)?;
    require_pin_match(
        "suite repository",
        &pins.suite_repository,
        &expected.suite_repository,
    )?;
    require_pin_match("suite commit", &pins.suite_commit, &expected.suite_commit)?;
    Ok(())
}

/// What a ref pin must be: either a fixed 40-char commit, or an exact tag.
/// Keying validation on this — not on a substring of the human-readable
/// `field` label — keeps a spelling coincidence (e.g. a label containing
/// "tag") from silently switching the provenance rule.
enum RefKind {
    Commit,
    Tag(&'static str),
}

fn reject_moving_ref(field: &str, value: &str, kind: RefKind) -> Result<()> {
    match kind {
        RefKind::Tag(expected) if value == expected => Ok(()),
        RefKind::Tag(expected) => Err(provenance_mismatch(format!(
            "{field} must be the immutable tag `{expected}`; \
             moving branches and alternate tags are insufficient provenance, found `{value}`"
        ))),
        RefKind::Commit if is_commit_sha(value) => Ok(()),
        RefKind::Commit => Err(provenance_mismatch(format!(
            "{field} must be a {COMMIT_LEN}-char lowercase hex pin; \
             moving branches and tags are insufficient provenance, found `{value}`"
        ))),
    }
}

fn reject_repository_substitution(repository: &str) -> Result<()> {
    if repository == COMPILER_REPOSITORY {
        return Ok(());
    }
    Err(provenance_mismatch(format!(
        "compiler repository must be `{COMPILER_REPOSITORY}`; \
         the published package `repository` field is not provenance, found `{repository}`"
    )))
}

fn require_pin_match(field: &str, actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(provenance_mismatch(format!(
            "{field} mismatch: expected `{expected}`, found `{actual}`"
        )))
    }
}

fn require_dependency_declaration(path: &Path, package_json: &Value) -> Result<String> {
    let dependencies = package_json
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            provenance_mismatch(format!("{}: missing `dependencies` object", path.display()))
        })?;
    match dependencies.get(NPM_NAME).and_then(Value::as_str) {
        Some(version) if !version.is_empty() => Ok(version.to_owned()),
        Some(_) => Err(provenance_mismatch(format!(
            "{}: `dependencies.typescript` must be a nonempty string",
            path.display()
        ))),
        None => Err(provenance_mismatch(format!(
            "{}: missing `dependencies.typescript`",
            path.display()
        ))),
    }
}

fn lock_root_declaration(path: &Path, lock: &Value) -> Result<String> {
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            provenance_mismatch(format!(
                "{}: package-lock.json missing `packages` object",
                path.display()
            ))
        })?;
    let root = packages.get("").ok_or_else(|| {
        provenance_mismatch(format!(
            "{}: package-lock.json missing root package entry",
            path.display()
        ))
    })?;
    let dependencies = root
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            provenance_mismatch(format!(
                "{}: root package-lock entry missing `dependencies`",
                path.display()
            ))
        })?;
    match dependencies.get(NPM_NAME).and_then(Value::as_str) {
        Some(version) if !version.is_empty() => Ok(version.to_owned()),
        Some(_) => Err(provenance_mismatch(format!(
            "{}: root `dependencies.typescript` must be a nonempty string",
            path.display()
        ))),
        None => Err(provenance_mismatch(format!(
            "{}: root package-lock missing `dependencies.typescript`",
            path.display()
        ))),
    }
}

fn lock_typescript_identity(path: &Path, lock: &Value) -> Result<(String, String)> {
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            provenance_mismatch(format!(
                "{}: package-lock.json missing `packages` object",
                path.display()
            ))
        })?;
    let entry = packages.get(LOCK_PACKAGE_KEY).ok_or_else(|| {
        provenance_mismatch(format!(
            "{}: missing locked package `{LOCK_PACKAGE_KEY}`",
            path.display()
        ))
    })?;
    let version = require_identity_string(path, entry, "version")?;
    let integrity = require_identity_string(path, entry, "integrity")?;
    if !integrity.starts_with("sha512-") {
        return Err(provenance_mismatch(format!(
            "{}: typescript integrity must be sha512-prefixed, found `{integrity}`",
            path.display()
        )));
    }
    Ok((version, integrity))
}

fn require_installed_repository_url(path: &Path, package: &Value) -> Result<String> {
    let repository = package.get("repository").ok_or_else(|| {
        provenance_mismatch(format!(
            "{}: missing `repository`; published repository identity is required and \
             cannot be inferred from compiler provenance",
            path.display()
        ))
    })?;
    match repository {
        Value::Object(map) => {
            let repo_type = match map.get("type").and_then(Value::as_str) {
                Some(value) if !value.is_empty() => value,
                Some(_) | None => {
                    return Err(provenance_mismatch(format!(
                        "{}: malformed `repository.type`",
                        path.display()
                    )));
                }
            };
            if repo_type != "git" {
                return Err(provenance_mismatch(format!(
                    "{}: repository.type mismatch: expected `git`, found `{repo_type}`",
                    path.display()
                )));
            }
            match map.get("url").and_then(Value::as_str) {
                Some(url) if !url.is_empty() => Ok(url.to_owned()),
                Some(_) | None => Err(provenance_mismatch(format!(
                    "{}: malformed `repository.url`",
                    path.display()
                ))),
            }
        }
        Value::String(url) if !url.is_empty() => Ok(url.clone()),
        Value::String(_) => Err(provenance_mismatch(format!(
            "{}: malformed empty `repository` string",
            path.display()
        ))),
        _ => Err(provenance_mismatch(format!(
            "{}: malformed `repository` field",
            path.display()
        ))),
    }
}

fn require_identity_string(path: &Path, value: &Value, field: &str) -> Result<String> {
    match value.get(field) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        Some(Value::String(_)) => Err(provenance_mismatch(format!(
            "{}: `{field}` must be a nonempty string",
            path.display()
        ))),
        Some(_) => Err(provenance_mismatch(format!(
            "{}: malformed `{field}` field",
            path.display()
        ))),
        None => Err(provenance_mismatch(format!(
            "{}: missing `{field}` field",
            path.display()
        ))),
    }
}

fn require_table_string(path: &Path, table: &Table, field: &str, scope: &str) -> Result<String> {
    match table.get(field) {
        Some(value) => require_value_string(path, value, &format!("{scope} `{field}`")),
        None => Err(provenance_mismatch(format!(
            "{}: {scope} missing `{field}` field",
            path.display()
        ))),
    }
}

fn require_named_string(path: &Path, table: &Table, field: &str, name: &str) -> Result<String> {
    match table.get(field) {
        Some(value) => require_value_string(path, value, &format!("source `{name}` `{field}`")),
        None => Err(provenance_mismatch(format!(
            "{}: source `{name}` missing `{field}` field",
            path.display()
        ))),
    }
}

fn require_value_string(path: &Path, value: &toml::Value, label: &str) -> Result<String> {
    match value.as_str() {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        Some(_) => Err(provenance_mismatch(format!(
            "{}: {label} must be a nonempty string",
            path.display()
        ))),
        None => Err(provenance_mismatch(format!(
            "{}: malformed {label}",
            path.display()
        ))),
    }
}

fn read_json_object(path: &Path) -> Result<Value> {
    let bytes = fs::read(path).map_err(|error| io_error(path, error))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| json_error(path, error))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(provenance_mismatch(format!(
            "{}: expected a JSON object",
            path.display()
        )))
    }
}

fn parse_toml<T: for<'de> Deserialize<'de>>(path: &Path, bytes: &[u8]) -> Result<T> {
    let input = std::str::from_utf8(bytes).map_err(|_| {
        VerificationError::new(
            ErrorCode::Toml,
            format!("{}: TOML must be UTF-8", path.display()),
        )
    })?;
    toml::from_str(input).map_err(|error| {
        // Identity-field problems are raised as provenance after a successful
        // document parse. A broken TOML document itself remains E_TOML.
        VerificationError::new(ErrorCode::Toml, format!("{}: {error}", path.display()))
    })
}

fn is_commit_sha(value: &str) -> bool {
    value.len() == COMMIT_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn provenance_mismatch(detail: impl Into<String>) -> VerificationError {
    VerificationError::new(ErrorCode::ProvenanceMismatch, detail)
}

fn io_error(path: &Path, error: std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

fn json_error(path: &Path, error: serde_json::Error) -> VerificationError {
    let code = match error.classify() {
        serde_json::error::Category::Io => ErrorCode::Io,
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => ErrorCode::Json,
        serde_json::error::Category::Data => ErrorCode::ProvenanceMismatch,
    };
    VerificationError::new(code, format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    const APPROVED_NPM_VERSION: &str = "7.0.2";
    const APPROVED_NPM_URL: &str = "https://registry.npmjs.org/typescript/-/typescript-7.0.2.tgz";
    const APPROVED_NPM_DIGEST_ALGORITHM: &str = "sha512";
    const APPROVED_NPM_DIGEST: &str =
        "8FYau96o3NKOhbjKi/qNvG/W5jhzxkbdm5sj9AbZ/5T5sWqn3hJgLfGx27sRKZWTvyzCP8dLRBTf5tBTSRVUNA==";
    const APPROVED_NPM_INTEGRITY: &str = "sha512-8FYau96o3NKOhbjKi/qNvG/W5jhzxkbdm5sj9AbZ/5T5sWqn3hJgLfGx27sRKZWTvyzCP8dLRBTf5tBTSRVUNA==";
    const APPROVED_COMPILER_COMMIT: &str = "2bd066d87f5bafd315be9f40889d0a60b9e58e0b";
    const APPROVED_COMPILER_TAG: &str = "typescript/v7.0.2";
    const APPROVED_COMPILER_URL: &str = "https://github.com/microsoft/typescript-go/archive/2bd066d87f5bafd315be9f40889d0a60b9e58e0b.tar.gz";
    const APPROVED_COMPILER_DIGEST_ALGORITHM: &str = "sha256";
    const APPROVED_COMPILER_DIGEST: &str =
        "5ccb47dbb3f68cd0da58b71e6f445eee36a50bd3e9f9f330cc23a97b88500119";
    const APPROVED_SUITE_COMMIT: &str = "4d4f005c8541e0255a9d8791205fdce326e462bc";
    const APPROVED_SUITE_URL: &str = "https://github.com/microsoft/TypeScript/archive/4d4f005c8541e0255a9d8791205fdce326e462bc.tar.gz";
    const APPROVED_SUITE_DIGEST_ALGORITHM: &str = "sha256";
    const APPROVED_SUITE_DIGEST: &str =
        "99cbcb9abf5308b15e90b991919b3f897918f5c19c13dce3a4d3cab618c64bf9";
    const APPROVED_PUBLISHED_REPOSITORY: &str = "https://github.com/microsoft/TypeScript.git";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "bamts-oracle-pins-{name}-{}-{epoch}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone)]
    struct FixtureSpec {
        npm_pin: String,
        npm_url: String,
        npm_digest_algorithm: String,
        npm_digest: String,
        npm_commit: String,
        compiler_pin: String,
        compiler_url: String,
        compiler_digest_algorithm: String,
        compiler_digest: String,
        compiler_commit: String,
        compiler_repository: Option<String>,
        suite_pin: String,
        suite_url: String,
        suite_digest_algorithm: String,
        suite_digest: String,
        suite_commit: String,
        suite_repository: Option<String>,
        omit_compiler_commit: bool,
        malformed_compiler_commit: bool,
        extra_sources_toml: String,
        package_declaration: String,
        lock_root_declaration: String,
        lock_version: String,
        lock_integrity: String,
        installed_name: String,
        installed_version: String,
        installed_git_head: Option<String>,
        installed_repository_url: Option<String>,
        installed_repository_type: Option<String>,
    }

    impl FixtureSpec {
        fn matching() -> Self {
            Self {
                npm_pin: APPROVED_NPM_VERSION.to_owned(),
                npm_url: APPROVED_NPM_URL.to_owned(),
                npm_digest_algorithm: APPROVED_NPM_DIGEST_ALGORITHM.to_owned(),
                npm_digest: APPROVED_NPM_DIGEST.to_owned(),
                npm_commit: APPROVED_COMPILER_COMMIT.to_owned(),
                compiler_pin: APPROVED_COMPILER_TAG.to_owned(),
                compiler_url: APPROVED_COMPILER_URL.to_owned(),
                compiler_digest_algorithm: APPROVED_COMPILER_DIGEST_ALGORITHM.to_owned(),
                compiler_digest: APPROVED_COMPILER_DIGEST.to_owned(),
                compiler_commit: APPROVED_COMPILER_COMMIT.to_owned(),
                compiler_repository: None,
                suite_pin: APPROVED_SUITE_COMMIT.to_owned(),
                suite_url: APPROVED_SUITE_URL.to_owned(),
                suite_digest_algorithm: APPROVED_SUITE_DIGEST_ALGORITHM.to_owned(),
                suite_digest: APPROVED_SUITE_DIGEST.to_owned(),
                suite_commit: APPROVED_SUITE_COMMIT.to_owned(),
                suite_repository: None,
                omit_compiler_commit: false,
                malformed_compiler_commit: false,
                extra_sources_toml: String::new(),
                package_declaration: APPROVED_NPM_VERSION.to_owned(),
                lock_root_declaration: APPROVED_NPM_VERSION.to_owned(),
                lock_version: APPROVED_NPM_VERSION.to_owned(),
                lock_integrity: APPROVED_NPM_INTEGRITY.to_owned(),
                installed_name: "typescript".to_owned(),
                installed_version: APPROVED_NPM_VERSION.to_owned(),
                installed_git_head: Some(APPROVED_COMPILER_COMMIT.to_owned()),
                installed_repository_url: Some(APPROVED_PUBLISHED_REPOSITORY.to_owned()),
                installed_repository_type: Some("git".to_owned()),
            }
        }
    }

    fn write_fixture(directory: &TestDirectory, spec: &FixtureSpec) {
        directory.write(SOURCES_PATH, &sources_toml(spec));
        directory.write(
            PACKAGE_JSON_PATH,
            &format!(
                "{{\n  \"dependencies\": {{\n    \"typescript\": \"{}\"\n  }}\n}}\n",
                spec.package_declaration
            ),
        );
        directory.write(PACKAGE_LOCK_PATH, &lockfile(spec));
        directory.write(TYPESCRIPT_PACKAGE_PATH, &installed_package(spec));
    }

    fn sources_toml(spec: &FixtureSpec) -> String {
        let compiler_commit_line = if spec.omit_compiler_commit {
            String::new()
        } else if spec.malformed_compiler_commit {
            "commit = 123\n".to_owned()
        } else {
            format!("commit = \"{}\"\n", spec.compiler_commit)
        };
        let compiler_repository_line = match &spec.compiler_repository {
            Some(repository) => format!("repository = \"{repository}\"\n"),
            None => String::new(),
        };
        let suite_repository_line = match &spec.suite_repository {
            Some(repository) => format!("repository = \"{repository}\"\n"),
            None => String::new(),
        };

        format!(
            r#"schema = "bamti.sources/v1"

[[source]]
name = "{SOURCE_NPM}"
pin = "{npm_pin}"
url = "{npm_url}"
digest_algorithm = "{npm_digest_algorithm}"
digest = "{npm_digest}"
commit = "{npm_commit}"

[[source]]
name = "{SOURCE_COMPILER}"
kind = "{SOURCE_KIND_GIT_ARCHIVE}"
pin = "{compiler_pin}"
url = "{compiler_url}"
digest_algorithm = "{compiler_digest_algorithm}"
digest = "{compiler_digest}"
{compiler_commit_line}{compiler_repository_line}
[[source]]
name = "{SOURCE_SUITE}"
kind = "{SOURCE_KIND_GIT_ARCHIVE}"
pin = "{suite_pin}"
url = "{suite_url}"
digest_algorithm = "{suite_digest_algorithm}"
digest = "{suite_digest}"
commit = "{suite_commit}"
{suite_repository_line}{extra_sources}"#,
            npm_pin = spec.npm_pin,
            npm_url = spec.npm_url,
            npm_digest_algorithm = spec.npm_digest_algorithm,
            npm_digest = spec.npm_digest,
            npm_commit = spec.npm_commit,
            compiler_pin = spec.compiler_pin,
            compiler_url = spec.compiler_url,
            compiler_digest_algorithm = spec.compiler_digest_algorithm,
            compiler_digest = spec.compiler_digest,
            suite_pin = spec.suite_pin,
            suite_url = spec.suite_url,
            suite_digest_algorithm = spec.suite_digest_algorithm,
            suite_digest = spec.suite_digest,
            suite_commit = spec.suite_commit,
            extra_sources = spec.extra_sources_toml,
        )
    }

    fn lockfile(spec: &FixtureSpec) -> String {
        format!(
            r#"{{
  "name": "fixture",
  "lockfileVersion": 3,
  "packages": {{
    "": {{
      "dependencies": {{
        "typescript": "{root_decl}"
      }}
    }},
    "node_modules/typescript": {{
      "version": "{version}",
      "resolved": "https://registry.npmjs.org/typescript/-/typescript-{version}.tgz",
      "integrity": "{integrity}"
    }}
  }}
}}
"#,
            root_decl = spec.lock_root_declaration,
            version = spec.lock_version,
            integrity = spec.lock_integrity,
        )
    }

    fn installed_package(spec: &FixtureSpec) -> String {
        let git_head = match &spec.installed_git_head {
            Some(git_head) => format!(",\n  \"gitHead\": \"{git_head}\""),
            None => String::new(),
        };
        let repository = match (
            &spec.installed_repository_type,
            &spec.installed_repository_url,
        ) {
            (Some(repo_type), Some(url)) => format!(
                ",\n  \"repository\": {{\n    \"type\": \"{repo_type}\",\n    \"url\": \"{url}\"\n  }}"
            ),
            (None, Some(url)) => format!(",\n  \"repository\": \"{url}\""),
            (None, None) => String::new(),
            (Some(_), None) => ",\n  \"repository\": {\n    \"type\": \"git\"\n  }".to_owned(),
        };
        format!(
            r#"{{
  "name": "{name}",
  "version": "{version}"{git_head}{repository}
}}
"#,
            name = spec.installed_name,
            version = spec.installed_version,
        )
    }

    fn assert_provenance_error(result: Result<OraclePins>) {
        let error = result.expect_err("fixture must fail provenance");
        assert_eq!(error.code(), ErrorCode::ProvenanceMismatch);
        assert_eq!(error.code().as_str(), "PROVENANCE_MISMATCH");
    }

    #[test]
    fn expected_pins_match_release_identity() {
        let pins = OraclePins::expected();
        assert_eq!(pins.npm_specifier, "typescript@7.0.2");
        assert_eq!(pins.npm_integrity, APPROVED_NPM_INTEGRITY);
        assert_eq!(
            pins.compiler_repository,
            "https://github.com/microsoft/typescript-go"
        );
        assert_eq!(pins.compiler_commit, APPROVED_COMPILER_COMMIT);
        assert_eq!(pins.compiler_tag, APPROVED_COMPILER_TAG);
        assert_eq!(
            pins.suite_repository,
            "https://github.com/microsoft/TypeScript"
        );
        assert_eq!(pins.suite_commit, APPROVED_SUITE_COMMIT);
    }

    #[test]
    fn verify_oracle_pins_accepts_matching_workspace() {
        let directory = TestDirectory::new("match");
        write_fixture(&directory, &FixtureSpec::matching());

        let pins = verify_oracle_pins(directory.path()).expect("matching pins");
        assert_eq!(pins, OraclePins::expected());
    }

    #[test]
    fn workspace_vendor_records_match_release_oracle() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(
            verify_oracle_pins(&root).expect("workspace source records match release oracle"),
            OraclePins::expected()
        );
    }

    #[test]
    fn rejects_package_json_range_declaration() {
        let directory = TestDirectory::new("pkg-range");
        let mut spec = FixtureSpec::matching();
        spec.package_declaration = "^7.0.2".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_lock_root_range_declaration() {
        let directory = TestDirectory::new("lock-root-range");
        let mut spec = FixtureSpec::matching();
        spec.lock_root_declaration = "^7.0.2".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_npm_version_mismatch() {
        let directory = TestDirectory::new("npm-version");
        let mut spec = FixtureSpec::matching();
        spec.npm_pin = "7.0.1".to_owned();
        spec.package_declaration = "7.0.1".to_owned();
        spec.lock_root_declaration = "7.0.1".to_owned();
        spec.lock_version = "7.0.1".to_owned();
        spec.installed_version = "7.0.1".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_npm_integrity_mismatch() {
        let directory = TestDirectory::new("npm-integrity");
        let mut spec = FixtureSpec::matching();
        spec.npm_digest =
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
                .to_owned();
        spec.lock_integrity = format!("sha512-{}", spec.npm_digest);
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_compiler_commit_mismatch() {
        let directory = TestDirectory::new("compiler-commit");
        let mut spec = FixtureSpec::matching();
        let bad = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
        spec.compiler_commit = bad.clone();
        spec.npm_commit = bad.clone();
        spec.installed_git_head = Some(bad);
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_compiler_tag_mismatch() {
        let directory = TestDirectory::new("compiler-tag");
        let mut spec = FixtureSpec::matching();
        spec.compiler_pin = "v7.0.2".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_suite_commit_mismatch() {
        let directory = TestDirectory::new("suite-commit");
        let mut spec = FixtureSpec::matching();
        let bad = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        spec.suite_commit = bad.clone();
        spec.suite_pin = bad;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_suite_repository_mismatch() {
        let directory = TestDirectory::new("suite-repository");
        let mut spec = FixtureSpec::matching();
        spec.suite_repository = Some(COMPILER_REPOSITORY.to_owned());
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_compiler_digest_mismatch() {
        let directory = TestDirectory::new("compiler-digest");
        let mut spec = FixtureSpec::matching();
        spec.compiler_digest =
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_moving_branch_compiler_ref() {
        for branch in ["main", "tsgo-port", "typescript/v7.0.2"] {
            let directory = TestDirectory::new(&format!("branch-compiler-{branch}"));
            let mut spec = FixtureSpec::matching();
            // `typescript/v7.0.2` is a valid tag pin but never a commit pin.
            spec.compiler_commit = branch.to_owned();
            spec.npm_commit = branch.to_owned();
            spec.installed_git_head = Some(branch.to_owned());
            write_fixture(&directory, &spec);
            assert_provenance_error(verify_oracle_pins(directory.path()));
        }
    }

    #[test]
    fn rejects_moving_branch_suite_ref() {
        let directory = TestDirectory::new("branch-suite");
        let mut spec = FixtureSpec::matching();
        spec.suite_commit = "tsgo-port".to_owned();
        spec.suite_pin = "tsgo-port".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_moving_branch_compiler_tag() {
        let directory = TestDirectory::new("branch-tag");
        let mut spec = FixtureSpec::matching();
        spec.compiler_pin = "tsgo-port".to_owned();
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_repository_field_substitution() {
        let directory = TestDirectory::new("repository-substitution");
        let mut spec = FixtureSpec::matching();
        spec.compiler_url = format!("{SUITE_REPOSITORY}/archive/{COMPILER_COMMIT}.tar.gz");
        spec.compiler_repository = Some(SUITE_REPOSITORY.to_owned());
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_missing_git_head_even_with_published_repository() {
        let directory = TestDirectory::new("missing-githead");
        let mut spec = FixtureSpec::matching();
        spec.installed_git_head = None;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_missing_source_commit_field() {
        let directory = TestDirectory::new("missing-commit");
        let mut spec = FixtureSpec::matching();
        spec.omit_compiler_commit = true;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_malformed_source_commit_field() {
        let directory = TestDirectory::new("malformed-commit");
        let mut spec = FixtureSpec::matching();
        spec.malformed_compiler_commit = true;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_malformed_installed_repository_field() {
        let directory = TestDirectory::new("malformed-repository");
        let mut spec = FixtureSpec::matching();
        spec.installed_repository_type = Some("git".to_owned());
        spec.installed_repository_url = None;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn matching_workspace_keeps_published_repository_out_of_compiler_pin() {
        let directory = TestDirectory::new("ignore-repository");
        write_fixture(&directory, &FixtureSpec::matching());
        let pins = verify_oracle_pins(directory.path()).expect("git sources are provenance");
        assert_eq!(pins.compiler_repository, COMPILER_REPOSITORY);
        assert_ne!(pins.compiler_repository, SUITE_REPOSITORY);
        assert_ne!(pins.compiler_repository, PUBLISHED_REPOSITORY_URL);
    }

    #[test]
    fn matching_workspace_ignores_unrelated_incomplete_catalog_rows() {
        let directory = TestDirectory::new("extra-catalog");
        let mut spec = FixtureSpec::matching();
        spec.extra_sources_toml = r#"
[[source]]
name = "node-headers"
pin = "24.18.0"
url = "https://nodejs.org/dist/v24.18.0/node-v24.18.0-headers.tar.gz"
digest_algorithm = "sha256"
digest = "6c7d41d83c3481d2301115b8ce4a44b7d4fbfa52859b1aac14f445d460137887"
"#
        .to_owned();
        write_fixture(&directory, &spec);
        let pins = verify_oracle_pins(directory.path()).expect("oracle sources only");
        assert_eq!(pins, OraclePins::expected());
    }

    #[test]
    fn rejects_source_kind_mismatch() {
        let directory = TestDirectory::new("kind-mismatch");
        let mut spec = FixtureSpec::matching();
        spec.extra_sources_toml = String::new();
        // Overwrite compiler kind by rewriting sources after the helper defaults.
        write_fixture(&directory, &spec);
        let mut sources = sources_toml(&spec);
        sources = sources.replace(
            &format!(
                r#"name = "{SOURCE_COMPILER}"
kind = "{SOURCE_KIND_GIT_ARCHIVE}""#
            ),
            &format!(
                r#"name = "{SOURCE_COMPILER}"
kind = "{SOURCE_KIND_NPM}""#
            ),
        );
        directory.write(SOURCES_PATH, &sources);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }

    #[test]
    fn rejects_missing_installed_repository_field() {
        let directory = TestDirectory::new("missing-repository");
        let mut spec = FixtureSpec::matching();
        spec.installed_repository_type = None;
        spec.installed_repository_url = None;
        write_fixture(&directory, &spec);
        assert_provenance_error(verify_oracle_pins(directory.path()));
    }
}
