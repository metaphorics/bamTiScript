use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const EXPECTED_STDOUT: &[u8] = b"hello from bamts\n";
static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

#[test]
fn run_fixture_preserves_stdout_and_exit_code() {
    let output = Command::new(bamts_binary())
        .args(["run", "--target", "jit"])
        .arg(fixture())
        .output()
        .expect("bamts run starts");
    assert_success(&output, "bamts run");
    assert_eq!(output.stdout, EXPECTED_STDOUT);
}

#[test]
fn aot_fixture_matches_jit_stdout_and_exit_code() {
    let directory = ScratchDirectory::new();
    let executable = directory
        .path
        .join(format!("hello{}", std::env::consts::EXE_SUFFIX));
    let compile = Command::new(bamts_binary())
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg(fixture())
        .env("BAMTS_CACHE_DIR", directory.path.join("cache"))
        .output()
        .expect("bamts compile starts");
    assert_success(&compile, "bamts compile --target aot");

    let output = Command::new(&executable)
        .output()
        .expect("compiled executable starts");
    assert_success(&output, "compiled fixture");
    assert_eq!(output.stdout, EXPECTED_STDOUT);
}

fn bamts_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bamts")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.ts")
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct ScratchDirectory {
    path: PathBuf,
}

impl ScratchDirectory {
    fn new() -> Self {
        let root = std::env::temp_dir();
        for _ in 0..128 {
            let index = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("bamts-cli-test-{}-{index}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create `{}`: {error}", path.display()),
            }
        }
        panic!("could not allocate a unique CLI test directory");
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
