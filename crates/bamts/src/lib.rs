//! The supported public entry point for compiling and running BamTS programs.
//!
//! The component crates remain available through the modules re-exported here;
//! applications can depend on `bamts` without coupling to the internal native
//! ABI crate.

#![forbid(unsafe_code)]

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use bamts_compiler::diagnostic::{Diagnostic, DiagnosticSeverity};
use bamts_compiler::lower::LowerOptions;
use bamts_compiler::pipeline::{FrontendMode, compile_program_frontend};
use bamts_compiler::program::{ProgramLoadError, ProgramLoader, ProgramLowerError, lower_program};
use bamts_compiler::project::{ConfigError, ProjectConfig, ProjectRoot};

pub use bamts_bytecode as bytecode;
pub use bamts_compiler as compiler;
#[cfg(feature = "node-host")]
pub use bamts_node as node;
pub use bamts_runtime as runtime;

/// Native-code backends, available only when either native-code feature is enabled.
#[cfg(any(feature = "aot", feature = "host-jit"))]
pub use bamts_codegen as codegen;

/// The output observable to a program embedding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramOutput {
    /// Bytes written to standard output by the program.
    pub stdout: Vec<u8>,
    /// The program's requested process exit code.
    pub exit_code: i32,
}

/// A failure at the facade boundary.
#[derive(Debug)]
pub enum Error {
    /// The project configuration could not be read.
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The project configuration is invalid.
    ProjectConfig { path: PathBuf, source: ConfigError },
    /// The entrypoint or one of its dependencies could not be loaded.
    ProgramLoad(ProgramLoadError),
    /// The complete program frontend produced one or more error diagnostics.
    Diagnostics { diagnostics: Vec<Diagnostic> },
    /// The checked program cannot be represented in verified BamTS bytecode.
    Lower(ProgramLowerError),
    /// The verified program did not execute successfully.
    Runtime(bamts_runtime::RuntimeError),
    /// Native object emission failed.
    #[cfg(feature = "aot")]
    Aot(bamts_codegen::AotError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadConfig { path, source } => write!(
                formatter,
                "could not read project configuration `{}`: {source}",
                path.display()
            ),
            Self::ProjectConfig { path, source } => write!(
                formatter,
                "invalid project configuration `{}`: {source}",
                path.display()
            ),
            Self::ProgramLoad(error) => write!(formatter, "could not load program: {error}"),
            Self::Diagnostics { diagnostics } => write!(
                formatter,
                "program has {} error diagnostic(s)",
                diagnostics.len()
            ),
            Self::Lower(error) => write!(formatter, "could not compile program: {error}"),
            Self::Runtime(error) => write!(formatter, "program execution failed: {error}"),
            #[cfg(feature = "aot")]
            Self::Aot(error) => write!(formatter, "could not emit native object: {error}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::ReadConfig { source, .. } => Some(source),
            Self::ProjectConfig { source, .. } => Some(source),
            Self::ProgramLoad(error) => Some(error),
            Self::Diagnostics { .. } => None,
            Self::Lower(error) => Some(error),
            Self::Runtime(error) => Some(error),
            #[cfg(feature = "aot")]
            Self::Aot(error) => Some(error),
        }
    }
}

impl From<ProgramLoadError> for Error {
    fn from(error: ProgramLoadError) -> Self {
        Self::ProgramLoad(error)
    }
}

impl From<ProgramLowerError> for Error {
    fn from(error: ProgramLowerError) -> Self {
        Self::Lower(error)
    }
}

impl From<bamts_runtime::RuntimeError> for Error {
    fn from(error: bamts_runtime::RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// Result type returned by facade convenience entry points.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Compiles an entrypoint and its complete local module graph into one executable program.
///
/// ```
/// # fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
/// let directory = std::env::temp_dir().join(format!(
///     "bamts-facade-compile-{}",
///     std::process::id(),
/// ));
/// std::fs::create_dir_all(&directory)?;
/// let path = directory.join("main.ts");
/// std::fs::write(&path, "let answer = 42;")?;
/// let executable = bamts::compile_source_file(&path)?;
/// std::fs::remove_dir_all(directory)?;
/// assert_eq!(executable.wire().modules().len(), 1);
/// # Ok(())
/// # }
/// ```
pub fn compile_source_file(
    path: impl AsRef<Path>,
) -> Result<bamts_compiler::program::ExecutableProgram> {
    let path = canonical_entrypoint(path.as_ref())?;
    let config_path = path
        .ancestors()
        .skip(1)
        .map(|directory| directory.join("tsconfig.json"))
        .find(|candidate| candidate.is_file());
    let root_path = config_path
        .as_deref()
        .and_then(Path::parent)
        .or_else(|| path.parent())
        .ok_or_else(|| {
            Error::ProgramLoad(ProgramLoadError::InvalidRoot(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "entrypoint has no parent directory",
            )))
        })?;
    let root_path = fs::canonicalize(root_path)
        .map_err(|error| Error::ProgramLoad(ProgramLoadError::InvalidRoot(error)))?;
    let root = ProjectRoot::new(&root_path).map_err(|error| {
        Error::ProgramLoad(ProgramLoadError::InvalidRoot(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            error,
        )))
    })?;
    let config_path = config_path.unwrap_or_else(|| root_path.join("tsconfig.json"));
    let config_source = if config_path.is_file() {
        fs::read_to_string(&config_path).map_err(|source| Error::ReadConfig {
            path: config_path.clone(),
            source,
        })?
    } else {
        "{}".to_owned()
    };
    let config = ProjectConfig::parse(&root, &config_path, &config_source).map_err(|source| {
        Error::ProjectConfig {
            path: config_path,
            source,
        }
    })?;
    let resolved = ProgramLoader::new(&root, config.options())?.load(&path)?;
    let frontend = compile_program_frontend(&resolved, FrontendMode::Check);
    let diagnostics = frontend
        .modules()
        .iter()
        .flat_map(|module| module.diagnostics().iter())
        .filter(|diagnostic| diagnostic.severity() == DiagnosticSeverity::Error)
        .cloned()
        .collect::<Vec<_>>();
    if !diagnostics.is_empty() {
        return Err(Error::Diagnostics { diagnostics });
    }
    lower_program(
        &resolved,
        &frontend,
        LowerOptions {
            javascript_compatibility: true,
        },
    )
    .map_err(Error::from)
}

/// Runs an entrypoint and its complete local module graph with the deterministic
/// Node-compatible host.
///
/// ```
/// # fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
/// let path = std::env::temp_dir().join(format!(
///     "bamts-facade-run-{}.ts",
///     std::process::id(),
/// ));
/// std::fs::write(&path, "console.log(42);")?;
/// let output = bamts::run_program(&path)?;
/// std::fs::remove_file(path)?;
/// assert_eq!(output.stdout, b"42\n");
/// assert_eq!(output.exit_code, 0);
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "node-host")]
pub fn run_program(path: impl AsRef<Path>) -> Result<ProgramOutput> {
    let executable = compile_source_file(path)?;
    let mut host = bamts_node::NodeHost::new();
    bamts_runtime::run(
        executable.wire(),
        &mut host,
        &bamts_runtime::Limits::default(),
    )?;

    Ok(ProgramOutput {
        stdout: host.stdout().to_vec(),
        exit_code: host.exit_code(),
    })
}

/// Compiles an entrypoint and its complete local module graph into a relocatable
/// native object.
///
/// This entry point requires the `aot` feature because object emission is not
/// part of the default interpreter-only dependency closure.
///
/// ```no_run
/// # fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
/// let path = std::env::temp_dir().join(format!(
///     "bamts-facade-aot-{}.ts",
///     std::process::id(),
/// ));
/// std::fs::write(&path, "let answer = 42;")?;
/// let object = bamts::compile_native_object(&path, "x86_64")?;
/// std::fs::remove_file(path)?;
/// assert!(!object.bytes.is_empty());
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "aot")]
pub fn compile_native_object(
    path: impl AsRef<Path>,
    target: &str,
) -> Result<bamts_codegen::AotObject> {
    let executable = compile_source_file(path)?;
    bamts_codegen::compile_aot(executable.wire(), target).map_err(Error::Aot)
}

#[cfg(feature = "aot")]
impl From<bamts_codegen::AotError> for Error {
    fn from(error: bamts_codegen::AotError) -> Self {
        Self::Aot(error)
    }
}

fn canonical_entrypoint(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| Error::ProgramLoad(ProgramLoadError::InvalidRoot(error)))?
    };
    fs::canonicalize(&absolute).map_err(|source| {
        Error::ProgramLoad(ProgramLoadError::Read {
            path: absolute,
            source,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::compile_source_file;
    #[cfg(feature = "node-host")]
    use super::run_program;
    use std::error::Error;
    use std::path::{Path, PathBuf};

    fn fixture(name: &str) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
        let directory =
            std::env::temp_dir().join(format!("bamts-facade-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("dependency.ts"),
            "export let value = 1; value = 2;",
        )?;
        let entrypoint = directory.join("main.ts");
        std::fs::write(
            &entrypoint,
            "import { value } from './dependency.js'; console.log(value);",
        )?;
        Ok((directory, entrypoint))
    }

    fn remove_fixture(directory: &Path) -> Result<(), Box<dyn Error>> {
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn compiles_complete_program_with_module_local_function_ids() -> Result<(), Box<dyn Error>> {
        let (directory, entrypoint) = fixture("compile")?;
        let executable = compile_source_file(&entrypoint)?;

        assert_eq!(executable.wire().modules().len(), 2);
        assert!(
            executable
                .wire()
                .modules()
                .iter()
                .all(|module| module.code().entry().get() == 0)
        );
        remove_fixture(&directory)
    }

    #[cfg(feature = "node-host")]
    #[test]
    fn runs_two_module_program_with_live_imported_mutation() -> Result<(), Box<dyn Error>> {
        let (directory, entrypoint) = fixture("run")?;
        let output = run_program(&entrypoint)?;

        assert_eq!(output.stdout, b"2\n");
        assert_eq!(output.exit_code, 0);
        remove_fixture(&directory)
    }

    #[cfg(feature = "aot")]
    #[test]
    fn compiles_complete_program_to_native_object() -> Result<(), Box<dyn Error>> {
        let (directory, entrypoint) = fixture("aot")?;
        let object = super::compile_native_object(&entrypoint, "x86_64")?;

        assert!(!object.bytes.is_empty());
        remove_fixture(&directory)
    }
}
