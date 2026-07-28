use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = required_path("OUT_DIR");
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR does not have Cargo's profile/build/package/out shape");
    let dependencies = profile_dir.join("deps");
    let target = required("TARGET");
    let host = required("HOST");
    let archive_name = if target.contains("windows-msvc") {
        "bamts_node.lib"
    } else {
        "libbamts_node.a"
    };
    let archive = out_dir.join(archive_name);
    let wrapper = out_dir.join("embedded_node.rs");
    fs::write(&wrapper, wrapper_source()).expect("could not write bamts-node staticlib wrapper");

    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    match select_node_rlib(&rustc, &target, &dependencies, &wrapper, &out_dir) {
        Some(node_rlib) => {
            assemble_staticlib(&rustc, &target, &dependencies, &node_rlib, &archive, &wrapper);
        }
        None => write_check_placeholder(&dependencies, &archive),
    }

    cargo_line("rerun-if-changed=../bamts-node/src");
    cargo_line("rerun-if-changed=../bamts-node/Cargo.toml");
    cargo_line(&format!("rustc-env=BAMTS_NODE_STATICLIB={}", archive.display()));
    cargo_line(&format!("rustc-env=BAMTS_HOST_TARGET={host}"));
    cargo_line(&format!("rustc-env=BAMTS_BUILD_TARGET={target}"));
}

fn wrapper_source() -> &'static str {
    "extern crate bamts_node;\n#[used]\nstatic KEEP_NODE: fn() -> bamts_node::NodeHost = bamts_node::NodeHost::new;\n#[used]\nstatic KEEP_AOT_MAIN: extern \"C\" fn() -> i32 = bamts_node::main;\n"
}

fn select_node_rlib(
    rustc: &OsStr,
    target: &str,
    dependencies: &Path,
    wrapper: &Path,
    out_dir: &Path,
) -> Option<PathBuf> {
    let candidates = artifacts(dependencies, "libbamts_node-", "rlib");
    if candidates.is_empty() {
        return None;
    }
    let compatible = candidates
        .iter()
        .filter(|candidate| metadata_supports_aot_main(rustc, target, dependencies, candidate, wrapper, out_dir))
        .cloned()
        .collect::<Vec<_>>();
    Some(compatible.first().cloned().unwrap_or_else(|| {
        panic!(
            "no bamts-node rlib in `{}` has the metadata required for the aot-main entrypoint; candidates: {}",
            dependencies.display(),
            display_paths(&candidates)
        )
    }))
}

fn metadata_supports_aot_main(
    rustc: &OsStr,
    target: &str,
    dependencies: &Path,
    candidate: &Path,
    wrapper: &Path,
    out_dir: &Path,
) -> bool {
    let probe = out_dir.join(format!(
        "bamts-node-probe-{}.rmeta",
        candidate
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
    ));
    let output = Command::new(rustc)
        .arg("--crate-name")
        .arg("bamts_node_probe")
        .arg("--crate-type")
        .arg("lib")
        .arg("--emit=metadata")
        .arg("--edition=2024")
        .arg("--target")
        .arg(target)
        .arg("--extern")
        .arg(format!("bamts_node={}", candidate.display()))
        .arg("-L")
        .arg(format!("dependency={}", dependencies.display()))
        .arg("-o")
        .arg(&probe)
        .arg(wrapper)
        .output()
        .unwrap_or_else(|error| panic!("could not start `{}`: {error}", Path::new(rustc).display()));
    let _ = fs::remove_file(probe);
    output.status.success()
}

fn assemble_staticlib(
    rustc: &OsStr,
    target: &str,
    dependencies: &Path,
    node_rlib: &Path,
    archive: &Path,
    wrapper: &Path,
) {
    let mut command = Command::new(rustc);
    command
        .arg("--crate-name")
        .arg("bamts_node_embedded")
        .arg("--crate-type")
        .arg("staticlib")
        .arg("--edition=2024")
        .arg("--target")
        .arg(target)
        .arg("--extern")
        .arg(format!("bamts_node={}", node_rlib.display()))
        .arg("-L")
        .arg(format!("dependency={}", dependencies.display()))
        .arg("-C")
        .arg(if required("PROFILE") == "release" {
            "opt-level=3"
        } else {
            "opt-level=0"
        })
        .arg("-o")
        .arg(archive)
        .arg(wrapper);
    let output = command.output().unwrap_or_else(|error| {
        panic!("could not start `{}`: {error}", Path::new(rustc).display())
    });
    if !output.status.success() {
        panic!(
            "bamts-node staticlib assembly failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn write_check_placeholder(dependencies: &Path, archive: &Path) {
    if artifacts(dependencies, "libbamts_node-", "rmeta").len() != 1 {
        panic!(
            "bamts-node build dependency did not produce exactly one metadata artifact in `{}`",
            dependencies.display()
        );
    }
    fs::write(archive, b"!<arch>\n").unwrap_or_else(|error| {
        panic!(
            "could not create check-only runtime archive `{}`: {error}",
            archive.display()
        )
    });
}

fn artifacts(directory: &Path, prefix: &str, extension: &str) -> Vec<PathBuf> {
    let suffix = format!(".{extension}");
    let mut artifacts = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("could not inspect `{}`: {error}", directory.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.starts_with(prefix) && name.ends_with(&suffix)).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    artifacts.sort();
    artifacts
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("Cargo did not provide required {name}"))
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(required(name))
}

fn cargo_line(directive: &str) {
    writeln!(io::stdout().lock(), "cargo:{directive}").expect("could not emit Cargo directive");
}
