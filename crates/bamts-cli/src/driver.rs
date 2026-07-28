use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use bamts_compiler::lower::{LowerError, LowerOptions, lower};
use bamts_compiler::pipeline::{FrontendMode, FrontendRequest, compile_frontend};
use bamts_compiler::source::{ScriptKind, SourceId, SourceText};
use bamts_runtime::{Limits, run_linked_program};

use crate::args::{CliArgs, ExecutionTarget, Mode};
use crate::diagnostics::{self, DiagnosticSource};

const NODE_STATICLIB: &[u8] = include_bytes!(env!("BAMTS_NODE_STATICLIB"));
const HOST_TARGET: &str = env!("BAMTS_HOST_TARGET");
const BUILD_TARGET: &str = env!("BAMTS_BUILD_TARGET");

/// Bytes and process status produced by one successful CLI command.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct CommandOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// A typed failure from compilation, execution, artifact publication, or linking.
#[derive(Debug)]
pub enum DriverError {
    ReadSource {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedSourceExtension {
        path: PathBuf,
    },
    Diagnostics {
        rendered: String,
    },
    Lower(LowerError),
    Jit(bamts_codegen::JitError),
    Native(bamts_runtime::NativeError),
    Aot(bamts_codegen::AotError),
    MissingEntrypoint,
    MultipleCompileInputs,
    UnsupportedCompileTarget(ExecutionTarget),
    UnsupportedOutputOption(&'static str),
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    CacheArchive {
        path: PathBuf,
        source: io::Error,
    },
    WriteObject {
        path: PathBuf,
        source: io::Error,
    },
    ToolchainMissing {
        program: OsString,
    },
    ToolchainProbe {
        program: OsString,
        source: io::Error,
    },
    ToolchainRejected {
        program: OsString,
        status: ExitStatus,
    },
    LinkStart {
        program: OsString,
        source: io::Error,
    },
    LinkFailed {
        program: OsString,
        status: ExitStatus,
        stderr: String,
    },
    PublishExecutable {
        path: PathBuf,
        source: io::Error,
    },
    CrossTargetLink {
        host: &'static str,
        target: &'static str,
    },
}

impl DriverError {
    #[must_use]
    pub const fn rendered_diagnostic(&self) -> Option<&str> {
        match self {
            Self::Diagnostics { rendered } => Some(rendered.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadSource { path, source } => {
                write!(formatter, "could not read `{}`: {source}", path.display())
            }
            Self::UnsupportedSourceExtension { path } => write!(
                formatter,
                "source file `{}` has an unsupported extension",
                path.display()
            ),
            Self::Diagnostics { .. } => formatter.write_str("source contains error diagnostics"),
            Self::Lower(error) => write!(formatter, "source cannot be lowered: {error}"),
            Self::Jit(error) => write!(formatter, "JIT compilation failed: {error}"),
            Self::Native(error) => write!(formatter, "program execution failed: {error}"),
            Self::Aot(error) => write!(formatter, "AOT object emission failed: {error}"),
            Self::MissingEntrypoint => formatter.write_str("command requires an entrypoint"),
            Self::MultipleCompileInputs => {
                formatter.write_str("native compilation accepts exactly one entrypoint")
            }
            Self::UnsupportedCompileTarget(target) => write!(
                formatter,
                "compile target `{target}` does not produce a persistent artifact"
            ),
            Self::UnsupportedOutputOption(option) => {
                write!(
                    formatter,
                    "{option} is not supported for native compilation"
                )
            }
            Self::CreateDirectory { path, source } => write!(
                formatter,
                "could not create output directory `{}`: {source}",
                path.display()
            ),
            Self::CacheArchive { path, source } => write!(
                formatter,
                "could not materialize the embedded runtime at `{}`: {source}",
                path.display()
            ),
            Self::WriteObject { path, source } => {
                write!(
                    formatter,
                    "could not write native object `{}`: {source}",
                    path.display()
                )
            }
            Self::ToolchainMissing { program } => write!(
                formatter,
                "system C toolchain `{}` was not found",
                Path::new(program).display()
            ),
            Self::ToolchainProbe { program, source } => write!(
                formatter,
                "could not inspect system C toolchain `{}`: {source}",
                Path::new(program).display()
            ),
            Self::ToolchainRejected { program, status } => write!(
                formatter,
                "system C toolchain `{}` is unavailable ({status})",
                Path::new(program).display()
            ),
            Self::LinkStart { program, source } => write!(
                formatter,
                "could not start linker `{}`: {source}",
                Path::new(program).display()
            ),
            Self::LinkFailed {
                program,
                status,
                stderr,
            } => write!(
                formatter,
                "linker `{}` failed ({status}): {}",
                Path::new(program).display(),
                stderr.trim()
            ),
            Self::PublishExecutable { path, source } => write!(
                formatter,
                "could not publish executable `{}`: {source}",
                path.display()
            ),
            Self::CrossTargetLink { host, target } => write!(
                formatter,
                "native linking for Cargo target `{target}` is unsupported from host `{host}`"
            ),
        }
    }
}

impl Error for DriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadSource { source, .. }
            | Self::CreateDirectory { source, .. }
            | Self::CacheArchive { source, .. }
            | Self::WriteObject { source, .. }
            | Self::ToolchainProbe { source, .. }
            | Self::LinkStart { source, .. }
            | Self::PublishExecutable { source, .. } => Some(source),
            Self::Lower(error) => Some(error),
            Self::Jit(error) => Some(error),
            Self::Native(error) => Some(error),
            Self::Aot(error) => Some(error),
            Self::MissingEntrypoint
            | Self::UnsupportedSourceExtension { .. }
            | Self::Diagnostics { .. }
            | Self::MultipleCompileInputs
            | Self::UnsupportedCompileTarget(_)
            | Self::UnsupportedOutputOption(_)
            | Self::ToolchainMissing { .. }
            | Self::ToolchainRejected { .. }
            | Self::LinkFailed { .. }
            | Self::CrossTargetLink { .. } => None,
        }
    }
}

/// Executes one already-parsed CLI command.
pub fn execute(args: &CliArgs) -> Result<CommandOutcome, DriverError> {
    match args.mode {
        Mode::Check => check(args),
        Mode::Compile => compile(args),
        Mode::Run => run(args),
    }
}

fn check(args: &CliArgs) -> Result<CommandOutcome, DriverError> {
    let paths = input_paths(args);
    let mut units = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let source_id = SourceId::new(u32::try_from(index).unwrap_or(u32::MAX));
        units.push(frontend(path, source_id)?);
    }

    let diagnostics = units
        .iter()
        .flat_map(|unit| unit.output.diagnostics().iter().cloned())
        .collect::<Vec<_>>();
    let names = units
        .iter()
        .map(|unit| unit.path.to_string_lossy())
        .collect::<Vec<_>>();
    let sources = units
        .iter()
        .zip(&names)
        .map(|(unit, name)| DiagnosticSource {
            id: unit.source_id,
            name,
            text: &unit.source,
        })
        .collect::<Vec<_>>();
    let rendered = diagnostics::render(args.diagnostics_format, &diagnostics, &sources);
    if units.iter().any(|unit| unit.output.has_errors()) {
        return Err(DriverError::Diagnostics { rendered });
    }
    Ok(CommandOutcome {
        stderr: rendered.into_bytes(),
        ..CommandOutcome::default()
    })
}

fn compile(args: &CliArgs) -> Result<CommandOutcome, DriverError> {
    if !args.extra_inputs.is_empty() {
        return Err(DriverError::MultipleCompileInputs);
    }
    if args.target != ExecutionTarget::Aot {
        return Err(DriverError::UnsupportedCompileTarget(args.target));
    }
    if args.output.emit_declarations {
        return Err(DriverError::UnsupportedOutputOption("--emit-declarations"));
    }
    if args.output.source_maps {
        return Err(DriverError::UnsupportedOutputOption("--source-maps"));
    }

    let entrypoint = required_entrypoint(args)?;
    let unit = frontend(entrypoint, SourceId::new(0))?;
    let warnings = require_clean_frontend(args, &unit)?;
    let bytecode = lower(
        unit.output.source_file(),
        LowerOptions {
            javascript_compatibility: is_javascript(unit.script_kind),
        },
    )
    .map_err(DriverError::Lower)?;
    if BUILD_TARGET != HOST_TARGET {
        return Err(DriverError::CrossTargetLink {
            host: HOST_TARGET,
            target: BUILD_TARGET,
        });
    }
    let object = bamts_codegen::compile_aot(&bytecode, HOST_TARGET).map_err(DriverError::Aot)?;
    let destination = output_path(args, entrypoint)?;
    link_executable(&object.bytes, &destination)?;
    Ok(CommandOutcome {
        stderr: warnings.into_bytes(),
        ..CommandOutcome::default()
    })
}

fn run(args: &CliArgs) -> Result<CommandOutcome, DriverError> {
    let entrypoint = required_entrypoint(args)?;
    let unit = frontend(entrypoint, SourceId::new(0))?;
    let warnings = require_clean_frontend(args, &unit)?;
    let bytecode = lower(
        unit.output.source_file(),
        LowerOptions {
            javascript_compatibility: is_javascript(unit.script_kind),
        },
    )
    .map_err(DriverError::Lower)?;
    let program = bamts_codegen::compile_jit(&bytecode).map_err(DriverError::Jit)?;
    let mut host = bamts_node::NodeHost::new();
    host.set_argv(
        ["bamts".to_owned(), entrypoint.display().to_string()]
            .into_iter()
            .chain(args.program_args.iter().cloned()),
    );
    let outcome = run_linked_program(&bytecode, &program, &mut host, &Limits::default())
        .map_err(DriverError::Native)?;
    let mut stdout = host.stdout().to_vec();
    stdout.extend_from_slice(&outcome.stdout);
    let exit_code = if host.exit_code() == 0 {
        outcome.exit_code
    } else {
        host.exit_code()
    };
    let mut stderr = warnings.into_bytes();
    stderr.extend_from_slice(host.stderr());
    Ok(CommandOutcome {
        stdout,
        stderr,
        exit_code,
    })
}

struct FrontendUnit {
    path: PathBuf,
    source_id: SourceId,
    source: Arc<SourceText>,
    script_kind: ScriptKind,
    output: bamts_compiler::pipeline::FrontendOutput,
}

fn frontend(path: &Path, source_id: SourceId) -> Result<FrontendUnit, DriverError> {
    let source = fs::read_to_string(path).map_err(|source| DriverError::ReadSource {
        path: path.to_owned(),
        source,
    })?;
    let script_kind = script_kind(path)?;
    let source = Arc::new(SourceText::new(source));
    let output = compile_frontend(FrontendRequest {
        source_id,
        script_kind,
        source: Arc::clone(&source),
        mode: FrontendMode::Check,
    });
    Ok(FrontendUnit {
        path: path.to_owned(),
        source_id,
        source,
        script_kind,
        output,
    })
}

fn require_clean_frontend(args: &CliArgs, unit: &FrontendUnit) -> Result<String, DriverError> {
    let source_name = unit.path.to_string_lossy();
    let rendered = diagnostics::render(
        args.diagnostics_format,
        unit.output.diagnostics(),
        &[DiagnosticSource {
            id: unit.source_id,
            name: &source_name,
            text: &unit.source,
        }],
    );
    if unit.output.has_errors() {
        Err(DriverError::Diagnostics { rendered })
    } else {
        Ok(rendered)
    }
}

fn input_paths(args: &CliArgs) -> Vec<PathBuf> {
    args.entrypoint
        .iter()
        .chain(&args.extra_inputs)
        .map(PathBuf::from)
        .collect()
}

fn required_entrypoint(args: &CliArgs) -> Result<&Path, DriverError> {
    args.entrypoint
        .as_deref()
        .map(Path::new)
        .ok_or(DriverError::MissingEntrypoint)
}

fn script_kind(path: &Path) -> Result<ScriptKind, DriverError> {
    match path.extension().and_then(OsStr::to_str) {
        Some("js" | "mjs" | "cjs") => Ok(ScriptKind::JavaScript),
        Some("jsx") => Ok(ScriptKind::JavaScriptReact),
        Some("ts" | "mts" | "cts") => Ok(ScriptKind::TypeScript),
        Some("tsx") => Ok(ScriptKind::TypeScriptReact),
        Some("json") => Ok(ScriptKind::Json),
        _ => Err(DriverError::UnsupportedSourceExtension {
            path: path.to_owned(),
        }),
    }
}

const fn is_javascript(kind: ScriptKind) -> bool {
    matches!(kind, ScriptKind::JavaScript | ScriptKind::JavaScriptReact)
}

fn output_path(args: &CliArgs, entrypoint: &Path) -> Result<PathBuf, DriverError> {
    if let Some(file) = &args.output.file {
        return Ok(PathBuf::from(file));
    }
    let file_name = entrypoint
        .file_stem()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| OsStr::new("a"));
    let mut file_name = file_name.to_os_string();
    file_name.push(std::env::consts::EXE_SUFFIX);
    if let Some(directory) = &args.output.dir {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).map_err(|source| DriverError::CreateDirectory {
            path: directory.clone(),
            source,
        })?;
        return Ok(directory.join(file_name));
    }
    Ok(entrypoint.with_file_name(file_name))
}

fn link_executable(object: &[u8], destination: &Path) -> Result<(), DriverError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| DriverError::CreateDirectory {
        path: parent.to_owned(),
        source,
    })?;
    let archive = cached_node_archive()?;
    let compiler = discover_toolchain()?;
    let temporary = TemporaryLinkFiles::create(parent, destination)?;
    fs::write(&temporary.object, object).map_err(|source| DriverError::WriteObject {
        path: temporary.object.clone(),
        source,
    })?;

    let mut command = Command::new(&compiler);
    command
        .arg(&temporary.object)
        .arg(&archive)
        .arg("-o")
        .arg(&temporary.executable);
    if cfg!(target_os = "linux") {
        command.args(["-ldl", "-lpthread", "-lm"]);
    }
    let output = command.output().map_err(|source| DriverError::LinkStart {
        program: compiler.clone(),
        source,
    })?;
    if !output.status.success() {
        return Err(DriverError::LinkFailed {
            program: compiler,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    publish_linked_executable(&temporary.executable, destination)
}

fn discover_toolchain() -> Result<OsString, DriverError> {
    let program = std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"));
    probe_toolchain(program)
}

fn probe_toolchain(program: OsString) -> Result<OsString, DriverError> {
    let probe = Command::new(&program).arg("--version").output();
    match probe {
        Ok(output) if output.status.success() => Ok(program),
        Ok(output) => Err(DriverError::ToolchainRejected {
            program,
            status: output.status,
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(DriverError::ToolchainMissing { program })
        }
        Err(source) => Err(DriverError::ToolchainProbe { program, source }),
    }
}

fn cached_node_archive() -> Result<PathBuf, DriverError> {
    let cache_dir = cache_root().join("runtime");
    fs::create_dir_all(&cache_dir).map_err(|source| DriverError::CacheArchive {
        path: cache_dir.clone(),
        source,
    })?;
    let extension = if cfg!(target_env = "msvc") {
        "lib"
    } else {
        "a"
    };
    let path = cache_dir.join(format!(
        "bamts-node-{:016x}.{extension}",
        content_hash(NODE_STATICLIB)
    ));
    if path.is_file() {
        return Ok(path);
    }
    let temporary = path.with_extension(format!("{extension}.{}.tmp", std::process::id()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
    {
        Ok(mut file) => {
            write_cached_archive(&mut file, &temporary)?;
            match fs::rename(&temporary, &path) {
                Ok(()) => Ok(path),
                Err(_error) if path.is_file() => {
                    let _ = fs::remove_file(&temporary);
                    Ok(path)
                }
                Err(source) => Err(DriverError::CacheArchive { path, source }),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if path.is_file() {
                Ok(path)
            } else {
                Err(DriverError::CacheArchive {
                    path: temporary,
                    source: error,
                })
            }
        }
        Err(source) => Err(DriverError::CacheArchive {
            path: temporary,
            source,
        }),
    }
}

fn write_cached_archive(file: &mut File, path: &Path) -> Result<(), DriverError> {
    file.write_all(NODE_STATICLIB)
        .and_then(|()| file.sync_all())
        .map_err(|source| DriverError::CacheArchive {
            path: path.to_owned(),
            source,
        })
}

fn cache_root() -> PathBuf {
    if let Some(path) = std::env::var_os("BAMTS_CACHE_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("bamts");
    }
    if let Some(path) = std::env::var_os("HOME") {
        return PathBuf::from(path).join(".cache/bamts");
    }
    std::env::temp_dir().join("bamts-cache")
}

const fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

struct TemporaryLinkFiles {
    object: PathBuf,
    executable: PathBuf,
}

impl TemporaryLinkFiles {
    fn create(parent: &Path, destination: &Path) -> Result<Self, DriverError> {
        let stem = destination
            .file_name()
            .unwrap_or_else(|| OsStr::new("bamts-output"))
            .to_string_lossy();
        for attempt in 0_u32..128 {
            let prefix = format!(".{stem}.bamts-{}-{attempt}", std::process::id());
            let object = parent.join(format!("{prefix}.o"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&object)
            {
                Ok(_) => {
                    return Ok(Self {
                        object,
                        executable: parent.join(format!("{prefix}.out")),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(DriverError::WriteObject {
                        path: object,
                        source,
                    });
                }
            }
        }
        Err(DriverError::WriteObject {
            path: parent.to_owned(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary-name attempts exhausted",
            ),
        })
    }
}

impl Drop for TemporaryLinkFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.object);
        let _ = fs::remove_file(&self.executable);
    }
}

fn publish_linked_executable(temporary: &Path, destination: &Path) -> Result<(), DriverError> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(destination).map_err(|source| DriverError::PublishExecutable {
                path: destination.to_owned(),
                source,
            })?;
            fs::rename(temporary, destination).map_err(|source| DriverError::PublishExecutable {
                path: destination.to_owned(),
                source,
            })
        }
        Err(source) => Err(DriverError::PublishExecutable {
            path: destination.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{DriverError, content_hash, probe_toolchain};

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        assert_eq!(content_hash(b"bamts"), 0xd301_9081_dac2_4a42);
        assert_ne!(content_hash(b"bamts"), content_hash(b"bamti"));
    }

    #[test]
    fn missing_toolchain_is_typed() {
        let error = probe_toolchain("/definitely/not/a/c/compiler".into())
            .expect_err("missing compiler must fail");
        assert!(matches!(error, DriverError::ToolchainMissing { .. }));
    }

    #[test]
    fn cross_target_link_has_actionable_error() {
        let error = DriverError::CrossTargetLink {
            host: "x86_64-unknown-linux-gnu",
            target: "aarch64-unknown-linux-gnu",
        };
        assert_eq!(
            error.to_string(),
            "native linking for Cargo target `aarch64-unknown-linux-gnu` is unsupported from host `x86_64-unknown-linux-gnu`"
        );
    }
}
