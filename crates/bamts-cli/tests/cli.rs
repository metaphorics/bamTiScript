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

#[test]
fn jit_runs_two_module_program_with_live_imported_mutation() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export let value = 1; value = 2;\n");
    project.write(
        "main.ts",
        "import { value } from './dependency.js'; console.log(value);\n",
    );

    let output = Command::new(bamts_binary())
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT run starts");

    assert_success(&output, "bamts run two-module JIT");
    assert_eq!(output.stdout, b"2\n");
}

#[test]
fn aot_runs_two_module_program_with_live_imported_mutation() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export let value = 1; value = 2;\n");
    project.write(
        "main.ts",
        "import { value } from './dependency.js'; console.log(value);\n",
    );
    let executable = project
        .path
        .join(format!("two-module{}", std::env::consts::EXE_SUFFIX));

    let compile = Command::new(bamts_binary())
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg("main.ts")
        .current_dir(&project.path)
        .env("BAMTS_CACHE_DIR", project.path.join("cache"))
        .output()
        .expect("bamts AOT compile starts");
    assert_success(&compile, "bamts compile two-module AOT");

    let output = Command::new(&executable)
        .output()
        .expect("two-module AOT executable starts");
    assert_success(&output, "compiled two-module program");
    assert_eq!(output.stdout, b"2\n");
}

#[test]
fn check_reports_dependency_errors() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './dependency.ts';\n");
    project.write("dependency.ts", "const = 1;\n");

    let output = project.check("main.ts");

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("dependency.ts"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn check_loads_type_only_dependencies() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        "import type { Shape } from './types.ts';\nexport const loaded = true;\n",
    );
    project.write("types.ts", "export interface Shape { value: string }\n");

    assert_success(&project.check("main.ts"), "bamts check type-only graph");
}

#[test]
fn check_loads_diamond_graph_once() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './left.ts';\nimport './right.ts';\n");
    project.write("left.ts", "import './leaf.ts';\nexport const left = 1;\n");
    project.write("right.ts", "import './leaf.ts';\nexport const right = 2;\n");
    project.write("leaf.ts", "export const leaf = 3;\n");

    assert_success(&project.check("main.ts"), "bamts check diamond graph");
}

#[test]
fn check_accepts_module_cycles() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './a.ts';\n");
    project.write("a.ts", "import './b.ts';\nexport const a = 1;\n");
    project.write("b.ts", "import './a.ts';\nexport const b = 2;\n");

    assert_success(&project.check("main.ts"), "bamts check cyclic graph");
}

#[test]
fn check_applies_project_lint_config_to_dependencies() {
    let project = ScratchDirectory::new();
    project.write("bamts.toml", "[lints.rules]\nexplicit-any = \"deny\"\n");
    project.write("src/tsconfig.json", "{}\n");
    project.write("src/main.ts", "import '../dependency.ts';\n");
    project.write("dependency.ts", "export const value: any = 1;\n");

    let output = project.check_from("src", "main.ts");

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("dependency.ts"), "{stderr}");
    assert!(stderr.contains("BAMTS-W017"), "{stderr}");
}

#[test]
fn check_renders_multi_file_diagnostics_in_stable_source_order() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './first.ts';\nimport './second.ts';\n");
    project.write("first.ts", "const = 1;\n");
    project.write("second.ts", "const = 2;\n");

    let first = project.check("main.ts");
    let second = project.check("main.ts");

    assert!(!first.status.success());
    assert_eq!(first.stderr, second.stderr);
    let stderr = stderr(&first);
    let first_position = stderr.find("first.ts").expect("first diagnostic");
    let second_position = stderr.find("second.ts").expect("second diagnostic");
    assert!(first_position < second_position, "{stderr}");
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

    fn write(&self, relative: &str, source: &str) {
        let path = self.path.join(relative);
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("fixture directory is created");
        fs::write(path, source).expect("fixture source is written");
    }

    fn check(&self, entrypoint: &str) -> Output {
        Command::new(bamts_binary())
            .args(["check", "--diagnostics-format", "text", entrypoint])
            .current_dir(&self.path)
            .output()
            .expect("bamts check starts")
    }

    fn check_from(&self, directory: &str, entrypoint: &str) -> Output {
        Command::new(bamts_binary())
            .args(["check", "--diagnostics-format", "text", entrypoint])
            .current_dir(self.path.join(directory))
            .output()
            .expect("bamts check starts")
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
