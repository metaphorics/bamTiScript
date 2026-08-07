use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bamts::discover_project;
use bamts_compiler::lower::LowerOptions;
use bamts_compiler::pipeline::{
    FrontendMode, ProgramFrontendOutput, compile_program_frontend_with_lints,
};
use bamts_compiler::program::{
    ProgramLoadError, ProgramLoader, ProgramLowerError, ResolvedProgram, lower_program,
};
use bamts_compiler::{
    diagnostic::DiagnosticReport,
    lint::{LintOverride, LintProfile, LintTable},
    project::{ProjectConfig, ProjectRoot, parse_bamts_toml},
};
use bamts_runtime::{Limits, run_linked_program};

use crate::args::{ArgsError, CliArgs, ExecutionTarget, Mode};
use crate::diagnostics::{self, DiagnosticSource};

const NODE_STATICLIB: &[u8] = include_bytes!(env!("BAMTS_NODE_STATICLIB"));
const HOST_TARGET: &str = env!("BAMTS_HOST_TARGET");
const BUILD_TARGET: &str = env!("BAMTS_BUILD_TARGET");
static NEXT_CACHE_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

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
    Usage(ArgsError),
    LintConfig {
        path: PathBuf,
        message: String,
    },
    ProjectConfig {
        path: PathBuf,
        message: String,
    },
    ProgramLoad(ProgramLoadError),
    Lower(ProgramLowerError),
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
    UnsafeFallbackCacheRoot {
        path: PathBuf,
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

    #[must_use]
    pub const fn is_usage_error(&self) -> bool {
        matches!(self, Self::Usage(_))
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
            Self::Usage(error) => error.fmt(formatter),
            Self::LintConfig { path, message } => {
                write!(
                    formatter,
                    "could not load lint configuration `{}`: {message}",
                    path.display()
                )
            }
            Self::ProjectConfig { path, message } => write!(
                formatter,
                "could not load project configuration `{}`: {message}",
                path.display()
            ),
            Self::ProgramLoad(error) => write!(formatter, "could not load program: {error}"),
            Self::Lower(error) => write!(formatter, "source cannot be lowered: {error}"),
            Self::Jit(error) => write!(formatter, "JIT compilation failed: {error}"),
            Self::Native(error) => write!(formatter, "program execution failed: {error}"),
            Self::Aot(error) => write!(formatter, "AOT object emission failed: {error}"),
            Self::MissingEntrypoint => formatter.write_str("command requires an entrypoint"),
            Self::MultipleCompileInputs => {
                formatter.write_str("program commands accept exactly one entrypoint")
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
            Self::UnsafeFallbackCacheRoot { path } => write!(
                formatter,
                "fallback cache root `{}` is not a private directory owned by the current user",
                path.display()
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
            Self::ProgramLoad(error) => Some(error),
            Self::Jit(error) => Some(error),
            Self::Native(error) => Some(error),
            Self::Aot(error) => Some(error),
            Self::Usage(error) => Some(error),
            Self::MissingEntrypoint
            | Self::UnsupportedSourceExtension { .. }
            | Self::Diagnostics { .. }
            | Self::LintConfig { .. }
            | Self::ProjectConfig { .. }
            | Self::MultipleCompileInputs
            | Self::UnsupportedCompileTarget(_)
            | Self::UnsupportedOutputOption(_)
            | Self::ToolchainMissing { .. }
            | Self::ToolchainRejected { .. }
            | Self::LinkFailed { .. }
            | Self::CrossTargetLink { .. }
            | Self::UnsafeFallbackCacheRoot { .. } => None,
        }
    }
}

/// Per-phase wall times (and best-effort process peak RSS) recorded by the AOT
/// driver for one `run --target aot` invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AotPhaseTelemetry {
    /// Wall time to emit the object file, in fractional milliseconds.
    pub object_wall_ms: f64,
    /// Wall time to link the native executable, in fractional milliseconds.
    pub link_wall_ms: f64,
    /// Wall time to run the linked executable to completion, in fractional
    /// milliseconds.
    pub run_wall_ms: f64,
    /// Best-effort driver process peak RSS in KiB (`/proc/self/status`
    /// `VmHWM`); `0` when unavailable.
    pub rss_kb: u64,
}

/// Executes one already-parsed CLI command.
pub fn execute(args: &CliArgs) -> Result<CommandOutcome, DriverError> {
    execute_with_telemetry(args).map(|(outcome, _telemetry)| outcome)
}

/// [`execute`] plus AOT phase telemetry.
///
/// The returned telemetry is `Some` only for a `run --target aot` command,
/// which is the sole path with distinct object/link/run phases; every other
/// command returns `None`.
pub fn execute_with_telemetry(
    args: &CliArgs,
) -> Result<(CommandOutcome, Option<AotPhaseTelemetry>), DriverError> {
    match args.mode {
        Mode::Check | Mode::Compile | Mode::Run => {
            let frontend = load_program_frontend(args)?;
            match args.mode {
                Mode::Check => Ok((check(args, &frontend)?, None)),
                Mode::Compile => Ok((compile(args, &frontend)?, None)),
                Mode::Run => run(args, &frontend),
                Mode::Explain => unreachable!("explain handled without a program"),
            }
        }
        Mode::Explain => {
            let rule = args
                .explain_rule
                .as_deref()
                .ok_or(DriverError::Usage(ArgsError::MissingExplainRule))?;
            let explanation = crate::args::explain_rule(rule).map_err(DriverError::Usage)?;
            Ok((
                CommandOutcome {
                    stdout: explanation.into_bytes(),
                    ..CommandOutcome::default()
                },
                None,
            ))
        }
    }
}

fn levels(args: &CliArgs, project_root: &Path) -> Result<LintTable, DriverError> {
    let profile = if args.pedantic {
        LintProfile::Pedantic
    } else if args.strict {
        LintProfile::Strict
    } else {
        LintProfile::Default
    };
    let mut levels = LintTable::new(profile);
    let path = project_root.join("bamts.toml");
    if path.is_file() {
        let source = fs::read_to_string(&path).map_err(|source| DriverError::ReadSource {
            path: path.clone(),
            source,
        })?;
        let config = parse_bamts_toml(&source).map_err(|error| DriverError::LintConfig {
            path: path.clone(),
            message: error.to_string(),
        })?;
        levels
            .apply_config(&config)
            .map_err(forbidden_lint_override)?;
    }
    let overrides = args.lint_overrides.iter().map(|override_arg| {
        let flag = match override_arg.level {
            bamts_compiler::lint::LintLevel::Allow => "-A",
            bamts_compiler::lint::LintLevel::Warn => "-W",
            bamts_compiler::lint::LintLevel::Deny => "-D",
            bamts_compiler::lint::LintLevel::Forbid => "-F",
        };
        LintOverride::new(
            override_arg.selector.as_str(),
            override_arg.level,
            format!("{flag} {}", override_arg.selector),
        )
    });
    levels
        .apply_cli(overrides)
        .map_err(forbidden_lint_override)?;
    Ok(levels)
}

fn forbidden_lint_override(error: bamts_compiler::lint::ForbidOverrideError) -> DriverError {
    DriverError::Usage(ArgsError::ForbiddenLintOverride {
        rule: error.rule().slug().to_string(),
        forbidden_by: error.forbidden_by().to_string(),
        lowered_by: error.lowered_by().to_string(),
    })
}

fn lower_options(args: &CliArgs) -> LowerOptions {
    LowerOptions {
        javascript_compatibility: args.js_compat.enabled,
    }
}

fn check(args: &CliArgs, frontend: &LoadedProgramFrontend) -> Result<CommandOutcome, DriverError> {
    let rendered = render_program_diagnostics(args, frontend);
    if frontend
        .output
        .modules()
        .iter()
        .any(|module| module.has_errors())
    {
        return Err(DriverError::Diagnostics { rendered });
    }
    Ok(CommandOutcome {
        stderr: rendered.into_bytes(),
        ..CommandOutcome::default()
    })
}

fn compile(
    args: &CliArgs,
    frontend: &LoadedProgramFrontend,
) -> Result<CommandOutcome, DriverError> {
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
    let warnings = require_clean_frontend(args, frontend)?;
    let executable = lower_program(&frontend.program, &frontend.output, lower_options(args))
        .map_err(DriverError::Lower)?;
    if BUILD_TARGET != HOST_TARGET {
        return Err(DriverError::CrossTargetLink {
            host: HOST_TARGET,
            target: BUILD_TARGET,
        });
    }
    let object =
        bamts_codegen::compile_aot(executable.wire(), HOST_TARGET).map_err(DriverError::Aot)?;
    let destination = output_path(args, entrypoint)?;
    link_executable(&object.bytes, &destination)?;
    Ok(CommandOutcome {
        stderr: warnings.into_bytes(),
        ..CommandOutcome::default()
    })
}

fn run(
    args: &CliArgs,
    frontend: &LoadedProgramFrontend,
) -> Result<(CommandOutcome, Option<AotPhaseTelemetry>), DriverError> {
    let entrypoint = required_entrypoint(args)?;
    let warnings = require_clean_frontend(args, frontend)?;
    let executable = lower_program(&frontend.program, &frontend.output, lower_options(args))
        .map_err(DriverError::Lower)?;
    match args.target {
        ExecutionTarget::Jit => Ok((run_jit(args, entrypoint, warnings, &executable)?, None)),
        ExecutionTarget::Aot => {
            let (outcome, telemetry) = run_aot(args, entrypoint, warnings, &executable)?;
            Ok((outcome, Some(telemetry)))
        }
    }
}

fn run_jit(
    args: &CliArgs,
    entrypoint: &Path,
    warnings: String,
    executable: &bamts_compiler::program::ExecutableProgram,
) -> Result<CommandOutcome, DriverError> {
    let program = bamts_codegen::compile_jit(executable.wire()).map_err(DriverError::Jit)?;
    let mut host = bamts_node::NodeHost::new();
    host.set_script_compiler(Box::new(bamts::ScriptCompiler));
    host.set_argv(
        ["bamts".to_owned(), entrypoint.display().to_string()]
            .into_iter()
            .chain(args.program_args.iter().cloned()),
    );
    let outcome = run_linked_program(executable.wire(), &program, &mut host, &Limits::default())
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

fn run_aot(
    args: &CliArgs,
    entrypoint: &Path,
    warnings: String,
    executable: &bamts_compiler::program::ExecutableProgram,
) -> Result<(CommandOutcome, AotPhaseTelemetry), DriverError> {
    if BUILD_TARGET != HOST_TARGET {
        return Err(DriverError::CrossTargetLink {
            host: HOST_TARGET,
            target: BUILD_TARGET,
        });
    }
    let object_started = Instant::now();
    let object =
        bamts_codegen::compile_aot(executable.wire(), HOST_TARGET).map_err(DriverError::Aot)?;
    let object_wall_ms = elapsed_ms(object_started);
    let id = NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let root = cache_root()?;
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir).map_err(|source| DriverError::CreateDirectory {
        path: run_dir.clone(),
        source,
    })?;
    let destination = run_dir.join(format!(
        "bamts-run-{}-{id}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    require_within_cache_root(&destination, &root)?;
    let link_started = Instant::now();
    link_executable(&object.bytes, &destination)?;
    let link_wall_ms = elapsed_ms(link_started);
    let launch_token = format!("{}-{id}", std::process::id());
    let run_started = Instant::now();
    let output = Command::new(&destination)
        .env(bamts_node::AOT_ENTRYPOINT_ENV, entrypoint.as_os_str())
        .env(bamts_node::AOT_LAUNCH_TOKEN_ENV, &launch_token)
        .arg(launch_token)
        .args(&args.program_args)
        .output();
    let run_wall_ms = elapsed_ms(run_started);
    let _ = fs::remove_file(&destination);
    let output = output.map_err(|source| DriverError::LinkStart {
        program: destination.into_os_string(),
        source,
    })?;
    let mut stderr = warnings.into_bytes();
    stderr.extend_from_slice(&output.stderr);
    let telemetry = AotPhaseTelemetry {
        object_wall_ms,
        link_wall_ms,
        run_wall_ms,
        rss_kb: peak_rss_kb().unwrap_or(0),
    };
    Ok((
        CommandOutcome {
            stdout: output.stdout,
            stderr,
            exit_code: output.status.code().unwrap_or(1),
        },
        telemetry,
    ))
}

/// Fractional milliseconds elapsed since `started`.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

/// Best-effort driver process peak RSS, in KiB, from `/proc/self/status`
/// `VmHWM`. Returns `None` when the file or field is unavailable.
fn peak_rss_kb() -> Option<u64> {
    let content = fs::read_to_string("/proc/self/status").ok()?;
    content.lines().find_map(|line| {
        line.strip_prefix("VmHWM:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn required_entrypoint(args: &CliArgs) -> Result<&Path, DriverError> {
    args.entrypoint
        .as_deref()
        .map(Path::new)
        .ok_or(DriverError::MissingEntrypoint)
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
    let cache_dir = cache_root()?.join("runtime");
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
    let (temporary, mut file) = loop {
        let id = NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary =
            path.with_extension(format!("{extension}.{}.{}.tmp", std::process::id(), id));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(DriverError::CacheArchive {
                    path: temporary,
                    source,
                });
            }
        }
    };
    if let Err(error) = write_cached_archive(&mut file, &temporary) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    match fs::rename(&temporary, &path) {
        Ok(()) => Ok(path),
        Err(_error) if path.is_file() => {
            let _ = fs::remove_file(&temporary);
            Ok(path)
        }
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            Err(DriverError::CacheArchive { path, source })
        }
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

fn cache_root() -> Result<PathBuf, DriverError> {
    if let Some(path) = std::env::var_os("BAMTS_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("bamts"));
    }
    if let Some(path) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(path).join(".cache/bamts"));
    }
    ensure_private_fallback_cache_root(fallback_cache_root_path()?)
}

#[cfg(unix)]
fn fallback_cache_root_path() -> Result<PathBuf, DriverError> {
    let parent = validate_fallback_parent_chain(&std::env::temp_dir())?;
    Ok(parent.join(format!("bamts-cache-{}", fallback_cache_user_key()?)))
}

#[cfg(not(unix))]
fn fallback_cache_root_path() -> Result<PathBuf, DriverError> {
    Ok(std::env::temp_dir().join(format!("bamts-cache-{}", fallback_cache_user_key()?)))
}

#[cfg(unix)]
fn fallback_cache_user_key() -> Result<String, DriverError> {
    Ok(effective_uid()?.to_string())
}

#[cfg(not(unix))]
fn fallback_cache_user_key() -> Result<String, DriverError> {
    Ok(std::env::var_os("USERNAME")
        .or_else(|| std::env::var_os("USER"))
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_owned()))
}

fn ensure_private_fallback_cache_root(path: PathBuf) -> Result<PathBuf, DriverError> {
    #[cfg(unix)]
    let path = {
        let parent = path
            .parent()
            .ok_or_else(|| DriverError::UnsafeFallbackCacheRoot { path: path.clone() })?;
        let name = path
            .file_name()
            .ok_or_else(|| DriverError::UnsafeFallbackCacheRoot { path: path.clone() })?;
        validate_fallback_parent_chain(parent)?.join(name)
    };
    match create_private_fallback_dir(&path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(DriverError::CreateDirectory { path, source });
        }
    }
    validate_private_fallback_dir(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn create_private_fallback_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_fallback_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn validate_fallback_parent_chain(path: &Path) -> Result<PathBuf, DriverError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let canonical = fs::canonicalize(path).map_err(|source| DriverError::CreateDirectory {
        path: path.to_owned(),
        source,
    })?;
    let euid = effective_uid()?;
    for ancestor in canonical.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|source| DriverError::CreateDirectory {
                path: ancestor.to_owned(),
                source,
            })?;
        let mode = metadata.permissions().mode();
        let writable = mode & 0o022 != 0;
        let owner = metadata.uid();
        let protected_by_sticky_owner = mode & 0o1000 != 0 && (owner == 0 || owner == euid);
        if !metadata.is_dir() || (writable && !protected_by_sticky_owner) {
            return Err(DriverError::UnsafeFallbackCacheRoot {
                path: ancestor.to_owned(),
            });
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn validate_private_fallback_dir(path: &Path) -> Result<(), DriverError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path).map_err(|source| DriverError::CreateDirectory {
        path: path.to_owned(),
        source,
    })?;
    let euid = effective_uid()?;
    let permissions = metadata.permissions().mode() & 0o777;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != euid
        || permissions & 0o077 != 0
    {
        return Err(DriverError::UnsafeFallbackCacheRoot {
            path: path.to_owned(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_fallback_dir(path: &Path) -> Result<(), DriverError> {
    let metadata = fs::metadata(path).map_err(|source| DriverError::CreateDirectory {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(DriverError::UnsafeFallbackCacheRoot {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Probe the effective user id without `unsafe` through an atomic random file.
#[cfg(unix)]
fn effective_uid() -> Result<u32, DriverError> {
    use std::os::unix::fs::MetadataExt;

    let parent = std::env::temp_dir();
    let probe = tempfile::Builder::new()
        .prefix(".bamts-euid-")
        .tempfile_in(&parent)
        .map_err(|source| DriverError::CreateDirectory {
            path: parent,
            source,
        })?;
    probe
        .as_file()
        .metadata()
        .map(|metadata| metadata.uid())
        .map_err(|source| DriverError::CreateDirectory {
            path: probe.path().to_owned(),
            source,
        })
}

/// Verify that the resolved parent directory of `path` is genuinely inside
/// `root` on the filesystem, not just lexically. `root/run` being a symlink
/// into a directory somebody else controls would pass a pure `starts_with`
/// check; canonicalizing both paths and comparing catches that escape.
fn require_within_cache_root(path: &Path, root: &Path) -> Result<(), DriverError> {
    let parent = path
        .parent()
        .ok_or_else(|| DriverError::UnsafeFallbackCacheRoot {
            path: path.to_owned(),
        })?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|source| DriverError::CreateDirectory {
            path: parent.to_owned(),
            source,
        })?;
    let canonical_root = fs::canonicalize(root).map_err(|source| DriverError::CreateDirectory {
        path: root.to_owned(),
        source,
    })?;
    if canonical_parent.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(DriverError::UnsafeFallbackCacheRoot {
            path: path.to_owned(),
        })
    }
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

struct LoadedProgramFrontend {
    program: ResolvedProgram,
    output: ProgramFrontendOutput,
}

fn load_program_frontend(args: &CliArgs) -> Result<LoadedProgramFrontend, DriverError> {
    if !args.extra_inputs.is_empty() {
        return Err(DriverError::MultipleCompileInputs);
    }
    let entrypoint = required_entrypoint(args)?;
    let current_directory = std::env::current_dir().map_err(|source| DriverError::ReadSource {
        path: PathBuf::from("."),
        source,
    })?;
    let current_directory = fs::canonicalize(&current_directory)
        .map_err(|error| DriverError::ProgramLoad(ProgramLoadError::InvalidRoot(error)))?;
    let absolute_entrypoint = if entrypoint.is_absolute() {
        entrypoint.to_path_buf()
    } else {
        current_directory.join(entrypoint)
    };
    let absolute_entrypoint =
        fs::canonicalize(&absolute_entrypoint).map_err(|source| DriverError::ReadSource {
            path: absolute_entrypoint,
            source,
        })?;
    let fallback_root = if absolute_entrypoint.starts_with(&current_directory) {
        current_directory
    } else {
        absolute_entrypoint
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf()
    };
    let project = discover_project(&absolute_entrypoint, fallback_root);
    let canonical_root = fs::canonicalize(project.root())
        .map_err(|error| DriverError::ProgramLoad(ProgramLoadError::InvalidRoot(error)))?;
    let root = ProjectRoot::new(canonical_root).map_err(|error| {
        DriverError::ProgramLoad(ProgramLoadError::InvalidRoot(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            error,
        )))
    })?;
    let config_path = project.config().to_owned();
    let config_source = if config_path.is_file() {
        fs::read_to_string(&config_path).map_err(|source| DriverError::ReadSource {
            path: config_path.clone(),
            source,
        })?
    } else {
        "{}".to_owned()
    };
    let config = ProjectConfig::parse(&root, &config_path, &config_source).map_err(|error| {
        DriverError::ProjectConfig {
            path: config_path,
            message: error.to_string(),
        }
    })?;
    let loader = ProgramLoader::new(&root, config.options()).map_err(DriverError::ProgramLoad)?;
    let program = loader
        .load(&absolute_entrypoint)
        .map_err(DriverError::ProgramLoad)?;
    let levels = levels(args, root.path())?;
    let output = compile_program_frontend_with_lints(&program, FrontendMode::Check, &levels);
    Ok(LoadedProgramFrontend { program, output })
}

fn render_program_diagnostics(args: &CliArgs, frontend: &LoadedProgramFrontend) -> String {
    let diagnostics = frontend
        .output
        .modules()
        .iter()
        .flat_map(|module| module.diagnostics().iter().cloned())
        .collect::<Vec<_>>();
    let names = frontend
        .program
        .modules()
        .iter()
        .map(|module| module.path().to_string_lossy())
        .collect::<Vec<_>>();
    let sources = frontend
        .program
        .modules()
        .iter()
        .zip(&names)
        .map(|(module, name)| DiagnosticSource {
            id: module.source_id(),
            name,
            text: module.source(),
        })
        .collect::<Vec<_>>();
    diagnostics::render_report(
        args.diagnostics_format,
        &DiagnosticReport::new(&diagnostics),
        &sources,
        args.error_limit,
    )
}

fn require_clean_frontend(
    args: &CliArgs,
    frontend: &LoadedProgramFrontend,
) -> Result<String, DriverError> {
    let rendered = render_program_diagnostics(args, frontend);
    if frontend
        .output
        .modules()
        .iter()
        .any(|module| module.has_errors())
    {
        Err(DriverError::Diagnostics { rendered })
    } else {
        Ok(rendered)
    }
}

/// Compiles a source entrypoint through the same frontend, lint, and lowering
/// pipeline as [`execute`], returning the lowered executable program without
/// running or linking it.  This is the in-process interpreter seam used by the
/// corpus differential harness so that CLI-supplied lint overrides and
/// JavaScript-compatibility flags reach the lowering stage.
pub fn compile_program(
    args: &CliArgs,
) -> Result<bamts_compiler::program::ExecutableProgram, DriverError> {
    let frontend = load_program_frontend(args)?;
    require_clean_frontend(args, &frontend)?;
    lower_program(&frontend.program, &frontend.output, lower_options(args))
        .map_err(DriverError::Lower)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bamts_compiler::lint::{LintLevel, SourceDialect, rule_by_name};

    use crate::args::{ArgsError, parse_args};

    #[cfg(target_os = "linux")]
    use super::peak_rss_kb;
    use super::{
        DriverError, content_hash, execute_with_telemetry, levels, lower_options, probe_toolchain,
    };

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

    #[test]
    fn cli_rule_override_beats_a_later_group_override() {
        let args = parse_args([
            "check",
            "-A",
            "explicit-any",
            "-D",
            "escape-hatches",
            "main.ts",
        ])
        .expect("arguments parse");
        let table =
            levels(&args, Path::new("/definitely/no/bamts/config")).expect("overrides resolve");
        let explicit_any = rule_by_name("explicit-any").expect("registered rule").id();
        let implicit_any = rule_by_name("implicit-any").expect("registered rule").id();
        assert_eq!(table.level(explicit_any), LintLevel::Allow);
        assert_eq!(table.level(implicit_any), LintLevel::Deny);
    }

    #[test]
    fn lowering_forbid_is_a_typed_usage_error() {
        let args = parse_args([
            "check",
            "-F",
            "explicit-any",
            "-A",
            "explicit-any",
            "main.ts",
        ])
        .expect("arguments parse");
        assert!(matches!(
            levels(&args, Path::new("/definitely/no/bamts/config")),
            Err(DriverError::Usage(ArgsError::ForbiddenLintOverride { .. }))
        ));
    }

    #[test]
    fn lowering_options_follow_javascript_compatibility_selection() {
        let disabled = parse_args(["compile", "main.ts"]).expect("arguments parse");
        let enabled =
            parse_args(["compile", "--compat", "strict", "main.ts"]).expect("arguments parse");

        assert!(!lower_options(&disabled).javascript_compatibility);
        assert!(lower_options(&enabled).javascript_compatibility);
    }

    #[test]
    fn strict_cli_profile_keeps_javascript_rules_nonfatal() {
        let args = parse_args(["check", "--strict", "vendored.js"]).expect("arguments parse");
        let table =
            levels(&args, Path::new("/definitely/no/bamts/config")).expect("profile resolves");
        let footgun = rule_by_name("invalid-number-formatting-options")
            .expect("registered footgun")
            .id();
        let typescript_only = rule_by_name("explicit-any").expect("registered rule").id();
        assert_eq!(
            table.level_for_source(footgun, SourceDialect::JavaScript),
            LintLevel::Warn
        );
        assert_eq!(
            table.level_for_source(typescript_only, SourceDialect::JavaScript),
            LintLevel::Allow
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_cache_root_has_stable_per_user_path() {
        let parent = super::validate_fallback_parent_chain(&std::env::temp_dir()).unwrap();
        let expected = parent.join(format!("bamts-cache-{}", super::effective_uid().unwrap()));
        assert_eq!(super::fallback_cache_root_path().unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn private_fallback_cache_root_creates_owner_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let path = std::env::temp_dir().join(format!(
            "bamts-cli-private-cache-create-{}-{}",
            std::process::id(),
            super::NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        let root = super::ensure_private_fallback_cache_root(path.clone())
            .expect("private fallback root should be created");
        let metadata = std::fs::symlink_metadata(&root).expect("metadata");
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[cfg(unix)]
    #[test]
    fn private_fallback_cache_root_rejects_group_or_other_bits() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let path = std::env::temp_dir().join(format!(
            "bamts-cli-private-cache-unsafe-{}-{}",
            std::process::id(),
            super::NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("create fixture root");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o710);
        std::fs::set_permissions(&path, permissions).expect("chmod");
        let error = super::ensure_private_fallback_cache_root(path.clone())
            .expect_err("group/other bits must be rejected");
        assert!(matches!(error, DriverError::UnsafeFallbackCacheRoot { .. }));
        let _ = std::fs::remove_dir_all(&path);
    }

    #[cfg(unix)]
    #[test]
    fn private_fallback_cache_root_rejects_replaceable_parent() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let parent = std::env::temp_dir().join(format!(
            "bamts-cli-replaceable-parent-{}-{}",
            std::process::id(),
            super::NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).expect("create fixture parent");
        let mut permissions = std::fs::metadata(&parent).expect("metadata").permissions();
        permissions.set_mode(0o777);
        std::fs::set_permissions(&parent, permissions).expect("chmod");

        let error = super::ensure_private_fallback_cache_root(parent.join("cache"))
            .expect_err("replaceable parent must be rejected");

        assert!(matches!(error, DriverError::UnsafeFallbackCacheRoot { .. }));
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[cfg(unix)]
    #[test]
    fn require_within_cache_root_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        use std::sync::atomic::Ordering;

        let workspace = std::env::temp_dir().join(format!(
            "bamts-cli-containment-{}-{}",
            std::process::id(),
            super::NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&workspace);
        let root = workspace.join("root");
        let outside = workspace.join("outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&outside).expect("create outside");
        // `root/run` is a symlink to a directory outside the cache root.
        symlink(&outside, root.join("run")).expect("create escaping symlink");
        let destination = root.join("run").join("bamts-run-evil");
        let error = super::require_within_cache_root(&destination, &root)
            .expect_err("symlink escaping the cache root must be rejected");
        assert!(matches!(error, DriverError::UnsafeFallbackCacheRoot { .. }));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn require_within_cache_root_accepts_genuine_subdirectory() {
        use std::sync::atomic::Ordering;

        let workspace = std::env::temp_dir().join(format!(
            "bamts-cli-containment-ok-{}-{}",
            std::process::id(),
            super::NEXT_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&workspace);
        let root = workspace.join("root");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&run_dir).expect("create run dir");
        let destination = run_dir.join("bamts-run-ok");
        super::require_within_cache_root(&destination, &root)
            .expect("genuine subdirectory of the cache root must be accepted");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_entrypoint_uses_one_canonical_config_root()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let directory =
            std::env::temp_dir().join(format!("bamts-cli-symlink-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let real = directory.join("real");
        let alias = directory.join("alias");
        std::fs::create_dir_all(real.join("src"))?;
        std::fs::create_dir_all(&alias)?;
        std::fs::write(real.join("bamts.toml"), "")?;
        std::fs::write(real.join("src/main.ts"), "let answer = 42;")?;
        std::fs::write(alias.join("bamts.toml"), "this is not valid TOML = [")?;
        symlink(real.join("src/main.ts"), alias.join("main.ts"))?;

        let alias_entrypoint = alias.join("main.ts");
        let args = parse_args(["check", alias_entrypoint.to_str().expect("UTF-8 temp path")])?;
        let outcome = super::execute(&args)?;
        assert_eq!(outcome.exit_code, 0);

        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn run_executes_node_vm_scripts_with_the_linked_backend()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (
                "default-import",
                "import vm from 'node:vm'; process.stdout.write(String(vm.runInThisContext('1+1')) + '\\n');",
                b"2\n".as_slice(),
            ),
            (
                "named-import",
                "import { runInThisContext } from 'node:vm'; process.stdout.write(String(runInThisContext('1+1')) + '\\n');",
                b"2\n".as_slice(),
            ),
            (
                "syntax-error",
                "import vm from 'node:vm'; try { new vm.Script('('); } catch (error) { process.stdout.write(error.name + '\\n'); }",
                b"SyntaxError\n".as_slice(),
            ),
            (
                "escaped-function",
                "import vm from 'node:vm'; const script = new vm.Script('(function(){ return 42; })'); const f = script.runInThisContext(); process.stdout.write(String(f()) + '\\n');",
                b"42\n".as_slice(),
            ),
            (
                "construct-runner",
                "import vm from 'node:vm'; const runner = vm.runInThisContext; const before = runner.prototype; const after = {}; const options = { get filename() { runner.prototype = after; return 'changed.js'; } }; const fallback = new runner('1', options); const result = new runner('({ answer: 42 })'); process.stdout.write(String(Object.getPrototypeOf(fallback) === before) + ',' + String(runner.prototype === after) + ',' + String(result.answer) + '\\n');",
                b"true,true,42\n".as_slice(),
            ),
        ];

        for (name, source, expected_stdout) in cases {
            let directory =
                std::env::temp_dir().join(format!("bamts-cli-vm-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory)?;
            let entrypoint = directory.join("main.ts");
            std::fs::write(&entrypoint, source)?;

            let args = parse_args(["run", entrypoint.to_str().expect("UTF-8 temp path")])?;
            let outcome = super::execute(&args)?;
            assert_eq!(outcome.stdout, expected_stdout, "{name}");
            assert_eq!(outcome.exit_code, 0, "{name}");
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peak_rss_kb_reads_proc_status_on_linux() {
        let rss = peak_rss_kb().expect("VmHWM is readable on this Linux host");
        assert!(rss > 0, "peak RSS should be positive");
    }

    #[test]
    fn execute_with_telemetry_reports_no_phases_for_jit_run()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("bamts-cli-tel-jit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory)?;
        let entrypoint = directory.join("main.ts");
        std::fs::write(&entrypoint, "process.stdout.write('ok\\n');")?;

        let args = parse_args([
            "run",
            "--jit",
            entrypoint.to_str().expect("UTF-8 temp path"),
        ])?;
        let (outcome, telemetry) = execute_with_telemetry(&args)?;
        assert_eq!(outcome.stdout, b"ok\n");
        assert_eq!(outcome.exit_code, 0);
        // The JIT path has no distinct object/link/run phases.
        assert_eq!(telemetry, None);

        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn execute_with_telemetry_records_aot_phase_walls() -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("bamts-cli-tel-aot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory)?;
        let entrypoint = directory.join("main.ts");
        std::fs::write(&entrypoint, "process.stdout.write('ok\\n');")?;

        let args = parse_args([
            "run",
            "--target",
            "aot",
            entrypoint.to_str().expect("UTF-8 temp path"),
        ])?;
        let result = execute_with_telemetry(&args);
        std::fs::remove_dir_all(&directory)?;

        match result {
            Ok((outcome, telemetry)) => {
                assert_eq!(outcome.stdout, b"ok\n");
                assert_eq!(outcome.exit_code, 0);
                let telemetry = telemetry.expect("AOT run reports phase telemetry");
                assert!(telemetry.object_wall_ms > 0.0, "object phase timed");
                assert!(telemetry.link_wall_ms > 0.0, "link phase timed");
                assert!(telemetry.run_wall_ms > 0.0, "run phase timed");
                assert!(telemetry.object_wall_ms.is_finite());
            }
            // A host without a C toolchain (or a cross-compiled build) cannot
            // link natively; that is an environment limit, not a telemetry bug.
            Err(DriverError::ToolchainMissing { .. })
            | Err(DriverError::CrossTargetLink { .. }) => {}
            Err(other) => return Err(Box::new(other)),
        }
        Ok(())
    }
}
