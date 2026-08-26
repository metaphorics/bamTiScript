use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::{
    ErrorCode, Result, VerificationError,
    schema::{
        COMPAT_SOURCE, COMPAT_TESTS_SOURCE, LEGACY_SOURCE, LEGACY_TESTS_SOURCE, LOCKFILE_PATH,
        PRIMARY_SOURCE, PRIMARY_TESTS_SOURCE, SourcePin, TYPESCRIPT_COMMIT,
        TYPESCRIPT_NPM_INTEGRITY, TYPESCRIPT_RELEASE, TYPESCRIPT_VERSION, load_sources, parse_json,
        read_bytes, required_source,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityReport {
    pub release: String,
    pub checks: usize,
}

#[derive(Debug, Deserialize)]
struct PackageLock {
    packages: BTreeMap<String, LockedPackage>,
}

#[derive(Debug, Deserialize)]
struct LockedPackage {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
}

pub fn verify_authority(root: &Path, release: &str) -> Result<AuthorityReport> {
    if release != TYPESCRIPT_RELEASE {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("unsupported authority release `{release}`"),
        ));
    }

    let (sources, _) = load_sources(root)?;
    let primary = required_source(&sources, PRIMARY_SOURCE)?;
    let primary_tests = required_source(&sources, PRIMARY_TESTS_SOURCE)?;
    let compat = required_source(&sources, COMPAT_SOURCE)?;
    let compat_tests = required_source(&sources, COMPAT_TESTS_SOURCE)?;
    let legacy = required_source(&sources, LEGACY_SOURCE)?;
    let legacy_tests = required_source(&sources, LEGACY_TESTS_SOURCE)?;
    let lock = load_lockfile(root)?;
    let typescript = lock
        .packages
        .get("node_modules/typescript")
        .ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!("{LOCKFILE_PATH}: missing `node_modules/typescript`"),
            )
        })?;

    let mut checks = 0;
    checks += check_primary_pin(primary)?;
    checks += check_primary_tests(primary, primary_tests)?;
    checks += check_lockfile(primary, typescript)?;
    checks += check_historical(legacy, legacy_tests)?;
    checks += check_historical(compat, compat_tests)?;
    checks += 1;

    Ok(AuthorityReport {
        release: release.to_owned(),
        checks,
    })
}

fn check_primary_pin(primary: &SourcePin) -> Result<usize> {
    if primary.pin != TYPESCRIPT_VERSION {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "source `{PRIMARY_SOURCE}` pin `{}` is not `{TYPESCRIPT_VERSION}`",
                primary.pin
            ),
        ));
    }
    if primary.commit.as_deref() != Some(TYPESCRIPT_COMMIT) {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!("source `{PRIMARY_SOURCE}` commit is not the stable TypeScript commit"),
        ));
    }
    if primary.digest_algorithm != "sha512" || primary.digest != TYPESCRIPT_NPM_INTEGRITY {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "source `{PRIMARY_SOURCE}` digest is not the published TypeScript {TYPESCRIPT_VERSION} integrity"
            ),
        ));
    }
    let expected_url =
        format!("https://registry.npmjs.org/typescript/-/typescript-{TYPESCRIPT_VERSION}.tgz");
    if primary.url != expected_url {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "source `{PRIMARY_SOURCE}` url is not the TypeScript {TYPESCRIPT_VERSION} tarball"
            ),
        ));
    }
    Ok(4)
}

fn check_primary_tests(primary: &SourcePin, tests: &SourcePin) -> Result<usize> {
    let Some(primary_commit) = primary.commit.as_deref() else {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("source `{PRIMARY_SOURCE}` is missing a commit"),
        ));
    };
    if tests.commit.as_deref() != Some(primary_commit) || tests.pin != primary_commit {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            "typescript primary tests do not share the stable source commit",
        ));
    }
    if !tests
        .url
        .contains(&format!("TypeScript/archive/{primary_commit}"))
    {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "typescript primary tests url does not pin the shared source commit",
        ));
    }
    Ok(2)
}

fn check_lockfile(primary: &SourcePin, typescript: &LockedPackage) -> Result<usize> {
    if typescript.version.as_deref() != Some(TYPESCRIPT_VERSION) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("{LOCKFILE_PATH}: typescript version is not `{TYPESCRIPT_VERSION}`"),
        ));
    }
    let integrity = typescript.integrity.as_deref().ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Digest,
            format!("{LOCKFILE_PATH}: typescript is missing integrity"),
        )
    })?;
    let expected = format!("sha512-{}", primary.digest);
    if integrity != expected {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!("{LOCKFILE_PATH}: typescript integrity does not match `{PRIMARY_SOURCE}`"),
        ));
    }
    if let Some(resolved) = typescript.resolved.as_deref()
        && resolved != primary.url
    {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("{LOCKFILE_PATH}: typescript resolved url does not match `{PRIMARY_SOURCE}`"),
        ));
    }
    Ok(3)
}

fn check_historical(source: &SourcePin, tests: &SourcePin) -> Result<usize> {
    if source.pin.starts_with("7.") || is_oracle_source(source) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "historical TypeScript corpus cannot act as the primary oracle",
        ));
    }
    if source.pin == TYPESCRIPT_VERSION || source.commit.as_deref() == Some(TYPESCRIPT_COMMIT) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "historical TypeScript corpus cannot share the 7.0.2 oracle pin",
        ));
    }
    let commit = source.commit.as_deref().ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!("historical source `{}` is missing a commit", source.name),
        )
    })?;
    if tests.commit.as_deref() != Some(commit) || tests.pin != commit {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "historical tests `{}` do not share source `{}` commit",
                tests.name, source.name
            ),
        ));
    }
    if is_oracle_source(tests) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "historical TypeScript tests cannot act as the primary oracle",
        ));
    }
    Ok(4)
}

fn is_oracle_source(source: &SourcePin) -> bool {
    source.name == PRIMARY_SOURCE || source.name == PRIMARY_TESTS_SOURCE
}

fn load_lockfile(root: &Path) -> Result<PackageLock> {
    let path = root.join(LOCKFILE_PATH);
    let bytes = read_bytes(&path)?;
    parse_json(&path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        MANIFEST_PATH, SOURCES_PATH, VerificationManifest, upstream_source_name,
        validate_catalog_cross_pin,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn fixture_from_workspace() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "bamts-authority-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("vendor/libuv-1.52.1")).unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();
        let workspace = workspace_root();
        fs::copy(workspace.join(SOURCES_PATH), root.join(SOURCES_PATH)).unwrap();
        fs::copy(workspace.join(LOCKFILE_PATH), root.join(LOCKFILE_PATH)).unwrap();
        Fixture { root }
    }

    #[test]
    fn published_tarball_matches_lock() {
        let report = verify_authority(&workspace_root(), TYPESCRIPT_RELEASE).unwrap();
        assert_eq!(report.release, TYPESCRIPT_RELEASE);
        assert!(report.checks >= 10);
    }

    #[test]
    fn source_and_tests_share_commit() {
        let (sources, _) = load_sources(&workspace_root()).unwrap();
        let primary = required_source(&sources, PRIMARY_SOURCE).unwrap();
        let tests = required_source(&sources, PRIMARY_TESTS_SOURCE).unwrap();
        assert_eq!(primary.commit.as_deref(), Some(TYPESCRIPT_COMMIT));
        assert_eq!(tests.commit.as_deref(), Some(TYPESCRIPT_COMMIT));
        assert_eq!(tests.pin, TYPESCRIPT_COMMIT);
    }

    #[test]
    fn all_typescript_pins_agree() {
        let root = workspace_root();
        let (sources, _) = load_sources(&root).unwrap();
        let path = root.join(MANIFEST_PATH);
        let manifest: VerificationManifest =
            parse_json(&path, &read_bytes(&path).unwrap()).unwrap();
        for id in ["typescript-7.0.2", "typescript-6.0.2", "typescript-5.9.3"] {
            let catalog = manifest
                .catalogs
                .iter()
                .find(|catalog| catalog.id == id)
                .unwrap();
            let source_name = upstream_source_name(id).unwrap();
            validate_catalog_cross_pin(
                &path,
                catalog,
                required_source(&sources, source_name).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn legacy_inputs_cannot_be_oracles() {
        let (sources, _) = load_sources(&workspace_root()).unwrap();
        for (source_name, tests_name) in [
            (LEGACY_SOURCE, LEGACY_TESTS_SOURCE),
            (COMPAT_SOURCE, COMPAT_TESTS_SOURCE),
        ] {
            let source = required_source(&sources, source_name).unwrap();
            let tests = required_source(&sources, tests_name).unwrap();
            assert_ne!(source.pin, TYPESCRIPT_VERSION);
            assert_ne!(source.commit.as_deref(), Some(TYPESCRIPT_COMMIT));
            assert_ne!(tests.commit.as_deref(), Some(TYPESCRIPT_COMMIT));
            assert!(!is_oracle_source(source));
            assert!(!is_oracle_source(tests));
        }
        assert!(verify_authority(&workspace_root(), TYPESCRIPT_RELEASE).is_ok());
    }

    #[test]
    fn rejects_stale_or_mutated_authority() {
        let fixture = fixture_from_workspace();
        let mut sources = fs::read_to_string(fixture.root.join(SOURCES_PATH)).unwrap();
        sources = sources.replacen("pin = \"7.0.2\"", "pin = \"6.0.2\"", 1);
        fs::write(fixture.root.join(SOURCES_PATH), sources).unwrap();
        let error = verify_authority(&fixture.root, TYPESCRIPT_RELEASE).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Schema);

        let fixture = fixture_from_workspace();
        let mut sources = fs::read_to_string(fixture.root.join(SOURCES_PATH)).unwrap();
        sources = sources.replace(
            TYPESCRIPT_NPM_INTEGRITY,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
        );
        fs::write(fixture.root.join(SOURCES_PATH), sources).unwrap();
        let error = verify_authority(&fixture.root, TYPESCRIPT_RELEASE).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Digest);
    }
}
