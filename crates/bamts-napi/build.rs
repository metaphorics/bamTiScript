use std::env;
use std::io::{self, Write};

fn directive(key: &str, value: &str) {
    writeln!(io::stdout(), "cargo:{key}={value}").expect("write Cargo build directive");
}

fn main() {
    napi_build::setup();
    for key in [
        "BAMTI_RELEASE_PACKAGE_VERSION",
        "BAMTI_SOURCE_COMMIT",
        "BAMTI_BUILD_SET_ID",
        "BAMTI_RELEASE_ID",
        "BAMTI_TARGET",
        "BAMTI_ARTIFACT_KIND",
        "BAMTI_NATIVE_ABI",
        "BAMTI_CLI_PROTOCOL",
    ] {
        directive("rerun-if-env-changed", key);
    }

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo provides CARGO_PKG_VERSION");
    let release_version =
        env::var("BAMTI_RELEASE_PACKAGE_VERSION").unwrap_or_else(|_| version.clone());
    assert_eq!(
        release_version, version,
        "BAMTI_RELEASE_PACKAGE_VERSION must match CARGO_PKG_VERSION"
    );
    let source_commit =
        env::var("BAMTI_SOURCE_COMMIT").unwrap_or_else(|_| "development".to_owned());
    let build_set_id = env::var("BAMTI_BUILD_SET_ID").unwrap_or_else(|_| "local".to_owned());
    let target = env::var("BAMTI_TARGET")
        .or_else(|_| env::var("TARGET"))
        .expect("Cargo provides TARGET");
    let native_abi = env::var("BAMTI_NATIVE_ABI").unwrap_or_else(|_| "1".to_owned());
    let cli_protocol = env::var("BAMTI_CLI_PROTOCOL").unwrap_or_else(|_| "1".to_owned());
    let release_id = env::var("BAMTI_RELEASE_ID").unwrap_or_else(|_| {
        format!(
            "bamti/{version}/{source_commit}/native-abi-{native_abi}/cli-protocol-{cli_protocol}/{build_set_id}"
        )
    });
    let artifact_kind =
        env::var("BAMTI_ARTIFACT_KIND").unwrap_or_else(|_| "native-addon".to_owned());

    for (key, value) in [
        ("BAMTI_RELEASE_PACKAGE_VERSION", release_version),
        ("BAMTI_SOURCE_COMMIT", source_commit),
        ("BAMTI_BUILD_SET_ID", build_set_id),
        ("BAMTI_RELEASE_ID", release_id),
        ("BAMTI_TARGET", target),
        ("BAMTI_ARTIFACT_KIND", artifact_kind),
        ("BAMTI_NATIVE_ABI", native_abi),
        ("BAMTI_CLI_PROTOCOL", cli_protocol),
    ] {
        directive("rustc-env", &format!("{key}={value}"));
    }
}
